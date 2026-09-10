#![no_std]
#![no_main]

use defmt::{debug, info, warn};
use defmt_rtt as _;
use embassy_executor::Spawner;
use embassy_rp::adc::{Adc, Channel, Config as AdcConfig, InterruptHandler};
use embassy_rp::bind_interrupts;
use embassy_rp::clocks::clk_sys_freq;
use embassy_rp::gpio::{Level, Output, Pull};
use embassy_rp::pwm::{Config as PwmConfig, Pwm};
use embassy_time::{Duration, Ticker};
use libm::{ceilf, floorf, fmodf, sinf};
use panic_probe as _;

const INITIAL_CARRIER_FREQ_HZ: f32 = 20_000.0;
const CONTROL_PERIOD_S: f32 = 0.0001;
const CONTROL_PERIOD_US: u64 = 100;
const MAX_ELEC_FREQ_HZ: f32 = 380.0;
const FREQ_SLEW_HZ_PER_S: f32 = 260.0;
const CARRIER_SLEW_HZ_PER_S: f32 = 22_000.0;

const COMMAND_LPF_ALPHA: f32 = 0.04;
const STOP_ZONE_MAX: f32 = 0.18;
const POWER_ZONE_MIN: f32 = 0.42;
const HOLD_ZONE_LAUNCH_FREQ_HZ: f32 = 8.0;

const CARRIER_LOW_MAX_FREQ_HZ: f32 = 35.0;
const CARRIER_MID_LOW_MAX_FREQ_HZ: f32 = 95.0;
const CARRIER_MID_HIGH_MAX_FREQ_HZ: f32 = 180.0;
const CARRIER_ASYNC_BASE_HZ: f32 = 2_400.0;
const CARRIER_ASYNC_WOBBLE_HZ: f32 = 160.0;
const CARRIER_ASYNC_WOBBLE_FREQ_HZ: f32 = 7.0;
const CARRIER_MIN_HZ: f32 = 400.0;
const CARRIER_MAX_HZ: f32 = 20_000.0;
const TELEMETRY_INTERVAL_TICKS: u32 = 5_000;

const TWO_PI: f32 = 2.0 * core::f32::consts::PI;

#[derive(Clone, Copy, PartialEq, Eq)]
enum CommandZone {
    Stop,
    Coast,
    Power,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum CarrierMode {
    Async,
    Pulse9,
    Pulse5,
    Pulse3,
}

bind_interrupts!(
    struct Irqs {
        ADC_IRQ_FIFO => InterruptHandler;
    }
);

#[embassy_executor::main]
async fn main(_spawner: Spawner) {
    // RP2350周辺を初期化し、VVVF制御に使うI/Oを確保する。
    let p = embassy_rp::init(Default::default());
    info!("boot sys_hz={=u32}", clk_sys_freq());

    // DRV8313のnSLEEP。停止時はLowでゲート駆動を止めて待機電力と発熱を抑える。
    let mut drv_nsleep = Output::new(p.PIN_5, Level::Low);

    // マスコン入力(可変抵抗)をADC0から読む。
    let mut adc = Adc::new(p.ADC, Irqs, AdcConfig::default());
    let mut mascon = Channel::new_pin(p.PIN_26, Pull::None);

    // U/V相はPWM Slice1のA/B、W相はSlice2のAを使う。
    let mut pwm_uv_cfg = default_pwm_config();
    let mut pwm_w_cfg = pwm_uv_cfg.clone();

    let mut pwm_uv = Pwm::new_output_ab(p.PWM_SLICE1, p.PIN_2, p.PIN_3, pwm_uv_cfg.clone());
    let mut pwm_w = Pwm::new_output_a(p.PWM_SLICE2, p.PIN_4, pwm_w_cfg.clone());

    let mut command_filtered = 0.0f32;
    let mut elec_freq_hz = 0.0f32;
    let mut elec_theta = 0.0f32;
    let mut vvvf_clock_s = 0.0f32;
    let mut carrier_freq_hz = INITIAL_CARRIER_FREQ_HZ;
    let mut tick_count = 0u32;
    let mut driver_awake = false;
    let mut last_zone = CommandZone::Stop;
    let mut last_carrier_mode = carrier_mode_for_freq(elec_freq_hz);

    // 100usごと(10kHz)に制御を更新する固定周期ループ。
    let mut ticker = Ticker::every(Duration::from_micros(CONTROL_PERIOD_US));

    loop {
        ticker.next().await;
        tick_count = tick_count.wrapping_add(1);

        // 生ADC値を0.0..1.0へ正規化し、一次LPFでガタつきを抑える。
        let command = match adc.read(&mut mascon).await {
            Ok(raw) => mascon_read_norm(raw),
            Err(_) => {
                warn!("adc read error");
                0.0
            }
        };
        command_filtered += (command - command_filtered) * COMMAND_LPF_ALPHA;
        let zone = command_zone(command_filtered);
        if zone != last_zone {
            info!(
                "zone={=str} cmd_milli={=u16}",
                command_zone_name(zone),
                scale_unit(command_filtered)
            );
            last_zone = zone;
        }

        // マスコン帯域(停止/惰行/力行)から目標電気角周波数を決め、
        // 変化率を制限して急加減速による音と電流の暴れを抑える。
        let target_freq = command_target_freq(command_filtered, elec_freq_hz);
        let max_step = FREQ_SLEW_HZ_PER_S * CONTROL_PERIOD_S;
        let df = clampf(target_freq - elec_freq_hz, -max_step, max_step);
        elec_freq_hz = clampf(elec_freq_hz + df, 0.0, MAX_ELEC_FREQ_HZ);

        // 停止帯かつ十分低速ならドライバをスリープさせ、PWM出力を全相ゼロにする。
        if command_filtered <= STOP_ZONE_MAX && elec_freq_hz < 0.5 {
            if driver_awake {
                info!("driver=sleep");
                driver_awake = false;
            }
            drv_nsleep.set_low();
            pwm_uv_cfg.compare_a = 0;
            pwm_uv_cfg.compare_b = 0;
            pwm_w_cfg.compare_a = 0;
            pwm_uv.set_config(&pwm_uv_cfg);
            pwm_w.set_config(&pwm_w_cfg);
            continue;
        }

        if !driver_awake {
            info!("driver=awake");
            driver_awake = true;
        }
        drv_nsleep.set_high();
        vvvf_clock_s += CONTROL_PERIOD_S;

        // 信号周波数に応じてキャリアを非同期寄り/9x/5x/3xへ切り替える。
        // ここもスルー制限を入れ、キャリアジャンプを聴感上なめらかにする。
        let target_carrier_hz = carrier_target_hz(elec_freq_hz, vvvf_clock_s);
        let carrier_max_step = CARRIER_SLEW_HZ_PER_S * CONTROL_PERIOD_S;
        let d_carrier = clampf(
            target_carrier_hz - carrier_freq_hz,
            -carrier_max_step,
            carrier_max_step,
        );
        carrier_freq_hz = clampf(carrier_freq_hz + d_carrier, CARRIER_MIN_HZ, CARRIER_MAX_HZ);
        let carrier_mode = carrier_mode_for_freq(elec_freq_hz);
        if carrier_mode != last_carrier_mode {
            info!(
                "carrier={=str} elec_centi_hz={=u16}",
                carrier_mode_name(carrier_mode),
                scale_hz(elec_freq_hz)
            );
            last_carrier_mode = carrier_mode;
        }

        let (divider, top) = pwm_params(clk_sys_freq(), carrier_freq_hz);
        pwm_uv_cfg.divider = divider.into();
        pwm_w_cfg.divider = divider.into();
        pwm_uv_cfg.top = top;
        pwm_w_cfg.top = top;

        // 位相角を積分して電気角を生成。2πを超えたら剰余で折り返す。
        elec_theta += TWO_PI * elec_freq_hz * CONTROL_PERIOD_S;
        if elec_theta >= TWO_PI {
            elec_theta = fmodf(elec_theta, TWO_PI);
        }

        // V/fを基本に、ノッチ感と高調波/うなりを足してVVVFらしい音色を作る。
        let amp = vvvf_amplitude(command_filtered, vvvf_clock_s);
        let h5 = 0.11 * command_filtered;
        let h7 = 0.07 * command_filtered;
        let wobble = 0.04 * sinf(TWO_PI * 31.0 * vvvf_clock_s);

        let duty_u = phase_duty(elec_theta, amp, h5, h7, wobble);
        let duty_v = phase_duty(elec_theta - TWO_PI / 3.0, amp, h5, h7, wobble);
        let duty_w = phase_duty(elec_theta + TWO_PI / 3.0, amp, h5, h7, wobble);

        pwm_uv_cfg.compare_a = duty_to_counts(duty_u, pwm_uv_cfg.top);
        pwm_uv_cfg.compare_b = duty_to_counts(duty_v, pwm_uv_cfg.top);
        pwm_w_cfg.compare_a = duty_to_counts(duty_w, pwm_w_cfg.top);

        pwm_uv.set_config(&pwm_uv_cfg);
        pwm_w.set_config(&pwm_w_cfg);

        if tick_count % TELEMETRY_INTERVAL_TICKS == 0 {
            debug!(
                "telemetry cmd_milli={=u16} elec_centi_hz={=u16} carrier_hz={=u16}",
                scale_unit(command_filtered),
                scale_hz(elec_freq_hz),
                carrier_freq_hz as u16
            );
        }
    }
}

fn default_pwm_config() -> PwmConfig {
    // 起動直後は20kHz中心、デューティ50%で安全側に初期化。
    let mut cfg = PwmConfig::default();
    cfg.divider = 1u8.into();
    cfg.top = pwm_wrap(clk_sys_freq(), INITIAL_CARRIER_FREQ_HZ, 1.0);
    cfg.compare_a = cfg.top / 2;
    cfg.compare_b = cfg.top / 2;
    cfg
}

fn command_target_freq(command: f32, current_freq_hz: f32) -> f32 {
    // 入力を3帯域で解釈する:
    // - 停止帯: 0Hzへ
    // - 惰行帯: 既存周波数を保持(停止直後のみ最低発進周波数へ持ち上げ)
    // - 力行帯: 0..MAX_ELEC_FREQ_HZへ線形マップ
    if command <= STOP_ZONE_MAX {
        0.0
    } else if command < POWER_ZONE_MIN {
        if current_freq_hz < HOLD_ZONE_LAUNCH_FREQ_HZ {
            HOLD_ZONE_LAUNCH_FREQ_HZ
        } else {
            current_freq_hz
        }
    } else if command >= POWER_ZONE_MIN {
        let accel = clampf(
            (command - POWER_ZONE_MIN) / (1.0 - POWER_ZONE_MIN),
            0.0,
            1.0,
        );
        accel * MAX_ELEC_FREQ_HZ
    } else {
        current_freq_hz
    }
}

fn command_zone(command: f32) -> CommandZone {
    if command <= STOP_ZONE_MAX {
        CommandZone::Stop
    } else if command < POWER_ZONE_MIN {
        CommandZone::Coast
    } else {
        CommandZone::Power
    }
}

fn command_zone_name(zone: CommandZone) -> &'static str {
    match zone {
        CommandZone::Stop => "stop",
        CommandZone::Coast => "coast",
        CommandZone::Power => "power",
    }
}

fn carrier_target_hz(elec_freq_hz: f32, time_s: f32) -> f32 {
    // 低周波は非同期キャリア+軽い揺らぎで「ブーン」を作り、
    // 速度が上がると9倍/5倍/3倍の同期寄りパターンへ段階遷移させる。
    let carrier = if elec_freq_hz < CARRIER_LOW_MAX_FREQ_HZ {
        CARRIER_ASYNC_BASE_HZ
            + CARRIER_ASYNC_WOBBLE_HZ * sinf(TWO_PI * CARRIER_ASYNC_WOBBLE_FREQ_HZ * time_s)
    } else if elec_freq_hz < CARRIER_MID_LOW_MAX_FREQ_HZ {
        elec_freq_hz * 9.0
    } else if elec_freq_hz < CARRIER_MID_HIGH_MAX_FREQ_HZ {
        elec_freq_hz * 5.0
    } else {
        elec_freq_hz * 3.0
    };
    clampf(carrier, CARRIER_MIN_HZ, CARRIER_MAX_HZ)
}

fn carrier_mode_for_freq(elec_freq_hz: f32) -> CarrierMode {
    if elec_freq_hz < CARRIER_LOW_MAX_FREQ_HZ {
        CarrierMode::Async
    } else if elec_freq_hz < CARRIER_MID_LOW_MAX_FREQ_HZ {
        CarrierMode::Pulse9
    } else if elec_freq_hz < CARRIER_MID_HIGH_MAX_FREQ_HZ {
        CarrierMode::Pulse5
    } else {
        CarrierMode::Pulse3
    }
}

fn carrier_mode_name(mode: CarrierMode) -> &'static str {
    match mode {
        CarrierMode::Async => "async",
        CarrierMode::Pulse9 => "9x",
        CarrierMode::Pulse5 => "5x",
        CarrierMode::Pulse3 => "3x",
    }
}

fn pwm_params(sys_hz: u32, carrier_hz: f32) -> (u8, u16) {
    // 16bitカウンタに収まる分周値を算出し、その分周値でtopを再計算する。
    let carrier_hz = clampf(carrier_hz, CARRIER_MIN_HZ, CARRIER_MAX_HZ);
    let divider = ceilf((sys_hz as f32) / (carrier_hz * 65536.0)) as i32;
    let divider = divider.clamp(1, 255) as u8;
    let top = pwm_wrap(sys_hz, carrier_hz, divider as f32);
    (divider, top)
}

fn pwm_wrap(sys_hz: u32, pwm_freq_hz: f32, divider: f32) -> u16 {
    let wrap = (sys_hz as f32 / (divider * pwm_freq_hz) - 1.0) as i32;
    wrap.clamp(0, 65535) as u16
}

fn duty_to_counts(duty: f32, top: u16) -> u16 {
    let max = u32::from(top) + 1;
    let value = (duty * max as f32) as u32;
    value.min(u32::from(top)) as u16
}

fn mascon_read_norm(raw: u16) -> f32 {
    // 12bit ADC値を正規化。下端ノイズ帯はデッドゾーンとして0扱いにする。
    let mut x = raw as f32 / 4095.0;
    if x < 0.03 {
        x = 0.0;
    }
    clampf(x, 0.0, 1.0)
}

fn vvvf_amplitude(throttle: f32, time_s: f32) -> f32 {
    // 振幅はV/f基調 + ノッチ段 + うなり成分で構成し、過変調を防ぐため上下限で拘束。
    let notch = floorf(throttle * 5.0 + 0.5) / 5.0;
    let vf = 0.18 + 0.78 * throttle;
    let beat = 0.06 * sinf(TWO_PI * (13.0 + 28.0 * throttle) * time_s);
    let growl = 0.03 * sinf(TWO_PI * (2.2 + throttle * 6.0) * time_s);
    clampf(vf + 0.06 * notch + beat + growl, 0.12, 0.95)
}

fn phase_duty(theta: f32, amp: f32, h5: f32, h7: f32, wobble: f32) -> f32 {
    // 基本波に5次/7次高調波を重畳して相波形を作り、0.03..0.97へ収めてデッドタイム余裕を残す。
    let mut phase_wave = sinf(theta) + h5 * sinf(5.0 * theta) + h7 * sinf(7.0 * theta);
    phase_wave /= 1.18;
    let duty = 0.5 + 0.5 * amp * (phase_wave + wobble);
    clampf(duty, 0.03, 0.97)
}

fn clampf(v: f32, lo: f32, hi: f32) -> f32 {
    if v < lo {
        lo
    } else if v > hi {
        hi
    } else {
        v
    }
}

fn scale_unit(v: f32) -> u16 {
    (clampf(v, 0.0, 1.0) * 1000.0) as u16
}

fn scale_hz(v: f32) -> u16 {
    clampf(v * 100.0, 0.0, u16::MAX as f32) as u16
}
