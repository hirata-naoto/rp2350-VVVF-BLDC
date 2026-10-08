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

// PWMキャリアの初期値と制御周期。制御周期は電気角の積分やスルーレート計算にも使うため、
// CONTROL_PERIOD_US と CONTROL_PERIOD_S は同じ時間を異なる単位で表している。
const INITIAL_CARRIER_FREQ_HZ: f32 = 20_000.0;
const CONTROL_PERIOD_S: f32 = 0.0001;
const CONTROL_PERIOD_US: u64 = 100;

// 電気角周波数の上限と、1秒あたりに変化させる最大周波数。
// 周波数の急変を抑えることで、開ループ駆動時の電流変動や機械的な衝撃を軽減する。
const MAX_ELEC_FREQ_HZ: f32 = 380.0;
const FREQ_SLEW_HZ_PER_S: f32 = 260.0;
const CARRIER_SLEW_HZ_PER_S: f32 = 22_000.0;

// マスコンの平滑化係数と、正規化入力(0.0..1.0)を分けるしきい値。
// 停止帯と力行帯の間は惰行帯として扱い、周波数指令を保持する。
const COMMAND_LPF_ALPHA: f32 = 0.04;
const STOP_ZONE_MAX: f32 = 0.18;
const POWER_ZONE_MIN: f32 = 0.42;
const HOLD_ZONE_LAUNCH_FREQ_HZ: f32 = 8.0;

// 電気角周波数に応じてキャリア変調方式を切り替える境界値。
// 境界未満では非同期キャリア、その後は電気角周波数に対する倍率を段階的に下げる。
const CARRIER_LOW_MAX_FREQ_HZ: f32 = 35.0;
const CARRIER_MID_LOW_MAX_FREQ_HZ: f32 = 95.0;
const CARRIER_MID_HIGH_MAX_FREQ_HZ: f32 = 180.0;

// 低速域の非同期キャリアに重ねる周期的な揺らぎと、キャリア周波数の許容範囲。
const CARRIER_ASYNC_BASE_HZ: f32 = 2_400.0;
const CARRIER_ASYNC_WOBBLE_HZ: f32 = 160.0;
const CARRIER_ASYNC_WOBBLE_FREQ_HZ: f32 = 7.0;
const CARRIER_MIN_HZ: f32 = 400.0;
const CARRIER_MAX_HZ: f32 = 20_000.0;

// テレメトリを出力する周期制御ループの回数。10kHz動作時は約0.5秒ごと。
const TELEMETRY_INTERVAL_TICKS: u32 = 5_000;

// 電気角を0..2πの範囲に折り返す際に使う定数。
const TWO_PI: f32 = 2.0 * core::f32::consts::PI;

// マスコン入力の意味を制御ロジック内で明示するための状態。
#[derive(Clone, Copy, PartialEq, Eq)]
enum CommandZone {
    Stop,
    Coast,
    Power,
}

// ログ表示とキャリア周波数の決定で共有する変調モード。
#[derive(Clone, Copy, PartialEq, Eq)]
enum CarrierMode {
    Async,
    Pulse9,
    Pulse5,
    Pulse3,
}

// EmbassyのADCドライバが使うFIFO割り込みを登録する。
bind_interrupts!(
    struct Irqs {
        ADC_IRQ_FIFO => InterruptHandler;
    }
);

#[embassy_executor::main]
async fn main(_spawner: Spawner) {
    // RP2350の周辺回路を初期化する。以降のピン・PWM・ADC操作はこの初期化結果を使う。
    let p = embassy_rp::init(Default::default());
    info!("boot sys_hz={=u32}", clk_sys_freq());

    // DRV8313のnSLEEPをGP5に接続する。初期状態はLowにしてドライバを無効化し、
    // PWMや入力値の準備が整うまでモーター側へ駆動信号が出ないようにする。
    let mut drv_nsleep = Output::new(p.PIN_5, Level::Low);

    // GP26(ADC0)からマスコン用ボリュームの電圧を読み取る。外部プル抵抗は設定しない。
    let mut adc = Adc::new(p.ADC, Irqs, AdcConfig::default());
    let mut mascon = Channel::new_pin(p.PIN_26, Pull::None);

    // GP2/GP3をPWM Slice1のA/B出力(U/V相)、GP4をSlice2のA出力(W相)に割り当てる。
    // 各相は同じキャリア設定を使い、比較値だけを相ごとのデューティにする。
    let mut pwm_uv_cfg = default_pwm_config();
    let mut pwm_w_cfg = pwm_uv_cfg.clone();

    let mut pwm_uv = Pwm::new_output_ab(p.PWM_SLICE1, p.PIN_2, p.PIN_3, pwm_uv_cfg.clone());
    let mut pwm_w = Pwm::new_output_a(p.PWM_SLICE2, p.PIN_4, pwm_w_cfg.clone());

    // 制御ループをまたいで保持する状態。周波数・位相・時間は制御周期ごとに更新し、
    // last_* は状態変化時だけログを出すために直前の状態を記憶する。
    let mut command_filtered = 0.0f32;
    let mut elec_freq_hz = 0.0f32;
    let mut elec_theta = 0.0f32;
    let mut vvvf_clock_s = 0.0f32;
    let mut carrier_freq_hz = INITIAL_CARRIER_FREQ_HZ;
    let mut tick_count = 0u32;
    let mut driver_awake = false;
    let mut last_zone = CommandZone::Stop;
    let mut last_carrier_mode = carrier_mode_for_freq(elec_freq_hz);

    // Tickerで制御周期を一定に保つ。制御ループは10kHzで動作し、
    // ADC取得、指令更新、PWM設定を各ティックごとに行う。
    let mut ticker = Ticker::every(Duration::from_micros(CONTROL_PERIOD_US));

    loop {
        ticker.next().await;
        tick_count = tick_count.wrapping_add(1);

        // ADC値を0.0..1.0へ正規化し、一次LPFでボリュームの微小な揺れを抑える。
        // 読み取りに失敗した場合は安全側の停止指令として扱い、次の周期で再試行する。
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

        // 停止帯は0Hz、惰行帯は現在値の保持(停止直後なら発進用最低周波数)、
        // 力行帯は入力に応じた周波数を目標にする。目標へ一度に飛ばず、
        // 1周期あたりの変化量を制限して周波数指令を滑らかに追従させる。
        let target_freq = command_target_freq(command_filtered, elec_freq_hz);
        let max_step = FREQ_SLEW_HZ_PER_S * CONTROL_PERIOD_S;
        let df = clampf(target_freq - elec_freq_hz, -max_step, max_step);
        elec_freq_hz = clampf(elec_freq_hz + df, 0.0, MAX_ELEC_FREQ_HZ);

        // 停止指令中に電気周波数が十分低くなったらドライバをスリープさせる。
        // PWM比較値も全相0にしてからループ先頭へ戻り、位相・キャリア等の駆動計算を省く。
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

        // 電気周波数に応じて非同期/9倍/5倍/3倍相当のキャリア目標を選ぶ。
        // 目標キャリアにも独立したスルー制限をかけ、モード境界での急な変化を抑える。
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

        // 電気周波数を時間積分して電気角を進める。2πを超えた分は剰余で折り返し、
        // sin波の引数が大きくなり続けるのを防いで数値計算を安定させる。
        elec_theta += TWO_PI * elec_freq_hz * CONTROL_PERIOD_S;
        if elec_theta >= TWO_PI {
            elec_theta = fmodf(elec_theta, TWO_PI);
        }

        // 振幅はマスコンに応じたV/f風の値を基礎とする。5次・7次高調波と低周波の揺らぎを加え、
        // 厳密なモーター制御ではなくVVVF風の音色を得るための波形を構成する。
        let amp = vvvf_amplitude(command_filtered, vvvf_clock_s);
        let h5 = 0.11 * command_filtered;
        let h7 = 0.07 * command_filtered;
        let wobble = 0.04 * sinf(TWO_PI * 31.0 * vvvf_clock_s);

        // 三相の位相差は120度(2π/3)。同じ振幅・高調波成分を各相へ適用する。
        let duty_u = phase_duty(elec_theta, amp, h5, h7, wobble);
        let duty_v = phase_duty(elec_theta - TWO_PI / 3.0, amp, h5, h7, wobble);
        let duty_w = phase_duty(elec_theta + TWO_PI / 3.0, amp, h5, h7, wobble);

        pwm_uv_cfg.compare_a = duty_to_counts(duty_u, pwm_uv_cfg.top);
        pwm_uv_cfg.compare_b = duty_to_counts(duty_v, pwm_uv_cfg.top);
        pwm_w_cfg.compare_a = duty_to_counts(duty_w, pwm_w_cfg.top);

        pwm_uv.set_config(&pwm_uv_cfg);
        pwm_w.set_config(&pwm_w_cfg);

        // 頻繁なログによる制御への影響を避けるため、状態遷移ログとは別に間引いて出力する。
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
    // PWMライブラリ既定値を土台に、初期キャリアを20kHzに設定する。
    // 比較値をカウンタ範囲の中央に置き、各相を中点電圧相当の50%デューティで初期化する。
    let mut cfg = PwmConfig::default();
    cfg.divider = 1u8.into();
    cfg.top = pwm_wrap(clk_sys_freq(), INITIAL_CARRIER_FREQ_HZ, 1.0);
    cfg.compare_a = cfg.top / 2;
    cfg.compare_b = cfg.top / 2;
    cfg
}

fn command_target_freq(command: f32, current_freq_hz: f32) -> f32 {
    // 正規化済みのマスコン入力を3帯域で解釈して、次周期の電気周波数目標を返す。
    // 停止帯では減速先を0Hzにし、惰行帯では速度を維持する。ただし停止状態から惰行へ
    // 移った直後は、回転を始めるための最低周波数まで指令を持ち上げる。
    // 力行帯ではしきい値から最大入力までを0..MAX_ELEC_FREQ_HZへ線形変換する。
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
    // ログ出力用に入力を停止・惰行・力行のいずれかへ分類する。
    // command_target_freqと同じ境界を使い、表示と制御の解釈を一致させる。
    if command <= STOP_ZONE_MAX {
        CommandZone::Stop
    } else if command < POWER_ZONE_MIN {
        CommandZone::Coast
    } else {
        CommandZone::Power
    }
}

fn command_zone_name(zone: CommandZone) -> &'static str {
    // defmtログに出せる静的文字列へ変換する。
    match zone {
        CommandZone::Stop => "stop",
        CommandZone::Coast => "coast",
        CommandZone::Power => "power",
    }
}

fn carrier_target_hz(elec_freq_hz: f32, time_s: f32) -> f32 {
    // 電気周波数が低い間は固定周波数を中心に正弦状の揺らぎを加え、非同期らしい音を作る。
    // 周波数帯が上がるとキャリア/電気周波数の比を9、5、3へ切り替え、
    // 最後にハードウェアで扱えるキャリア範囲へ制限して返す。
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
    // carrier_target_hzと同じ境界で現在の変調モードを分類する。
    // 周波数そのものの設定とは別に保持し、モード変更時だけログを記録する。
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
    // RTTログ向けの短いモード名を返す。
    match mode {
        CarrierMode::Async => "async",
        CarrierMode::Pulse9 => "9x",
        CarrierMode::Pulse5 => "5x",
        CarrierMode::Pulse3 => "3x",
    }
}

fn pwm_params(sys_hz: u32, carrier_hz: f32) -> (u8, u16) {
    // PWMカウンタは16bitなので、指定キャリアを生成できる最小の整数分周比を求める。
    // 分周比は周辺回路の設定範囲(1..255)へ収め、その実際の値からTOPを計算する。
    let carrier_hz = clampf(carrier_hz, CARRIER_MIN_HZ, CARRIER_MAX_HZ);
    let divider = ceilf((sys_hz as f32) / (carrier_hz * 65536.0)) as i32;
    let divider = divider.clamp(1, 255) as u8;
    let top = pwm_wrap(sys_hz, carrier_hz, divider as f32);
    (divider, top)
}

fn pwm_wrap(sys_hz: u32, pwm_freq_hz: f32, divider: f32) -> u16 {
    // カウンタが0からTOPまで数えるPWMを前提に、クロック/分周比/目標周波数からTOPを求める。
    // 表現可能な16bit範囲へ制限してから返す。
    let wrap = (sys_hz as f32 / (divider * pwm_freq_hz) - 1.0) as i32;
    wrap.clamp(0, 65535) as u16
}

fn duty_to_counts(duty: f32, top: u16) -> u16 {
    // 0..1のデューティ比をPWM比較値へ変換する。カウンタ周期はTOP+1カウントだが、
    // 比較値自体はTOPを超えないようにする。
    let max = u32::from(top) + 1;
    let value = (duty * max as f32) as u32;
    value.min(u32::from(top)) as u16
}

fn mascon_read_norm(raw: u16) -> f32 {
    // 12bit ADCの最大値4095を基準に、入力を0.0..1.0へ正規化する。
    // ごく低い領域はADCノイズやボリュームのずれを想定したデッドゾーンとして0にする。
    let mut x = raw as f32 / 4095.0;
    if x < 0.03 {
        x = 0.0;
    }
    clampf(x, 0.0, 1.0)
}

fn vvvf_amplitude(throttle: f32, time_s: f32) -> f32 {
    // throttleに応じて基本振幅を増やし、丸めた段階値でノッチ感を加える。
    // さらに時間依存のビート/うなり成分を混ぜ、過大・過小な変調にならない範囲へ制限する。
    let notch = floorf(throttle * 5.0 + 0.5) / 5.0;
    let vf = 0.18 + 0.78 * throttle;
    let beat = 0.06 * sinf(TWO_PI * (13.0 + 28.0 * throttle) * time_s);
    let growl = 0.03 * sinf(TWO_PI * (2.2 + throttle * 6.0) * time_s);
    clampf(vf + 0.06 * notch + beat + growl, 0.12, 0.95)
}

fn phase_duty(theta: f32, amp: f32, h5: f32, h7: f32, wobble: f32) -> f32 {
    // 基本正弦波に5次・7次高調波を加えて相波形を作り、係数でピークを整える。
    // 中心値0.5の周りへ振幅を適用し、端点付近の余裕を残すためデューティを0.03..0.97に制限する。
    let mut phase_wave = sinf(theta) + h5 * sinf(5.0 * theta) + h7 * sinf(7.0 * theta);
    phase_wave /= 1.18;
    let duty = 0.5 + 0.5 * amp * (phase_wave + wobble);
    clampf(duty, 0.03, 0.97)
}

fn clampf(v: f32, lo: f32, hi: f32) -> f32 {
    // f32値を指定範囲へ収める共通処理。各制御値が設定可能範囲を超えないように使う。
    if v < lo {
        lo
    } else if v > hi {
        hi
    } else {
        v
    }
}

fn scale_unit(v: f32) -> u16 {
    // 0.0..1.0の値をログ表示用の0..1000整数へ変換する。
    (clampf(v, 0.0, 1.0) * 1000.0) as u16
}

fn scale_hz(v: f32) -> u16 {
    // Hz値を小数第2位相当の整数(0.01Hz単位)へ変換し、defmtで表示しやすくする。
    clampf(v * 100.0, 0.0, u16::MAX as f32) as u16
}
