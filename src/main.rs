#![no_std]
#![no_main]

use embassy_executor::Spawner;
use embassy_rp::adc::{Adc, Channel, Config as AdcConfig, InterruptHandler};
use embassy_rp::bind_interrupts;
use embassy_rp::clocks::clk_sys_freq;
use embassy_rp::gpio::{Level, Output, Pull};
use embassy_rp::pwm::{Config as PwmConfig, Pwm};
use embassy_time::{Duration, Ticker};
use libm::{floorf, fmodf, sinf};
use panic_halt as _;

const PWM_FREQ_HZ: f32 = 20_000.0;
const CONTROL_PERIOD_S: f32 = 0.0001;
const CONTROL_PERIOD_US: u64 = 100;
const MAX_ELEC_FREQ_HZ: f32 = 380.0;
const FREQ_SLEW_HZ_PER_S: f32 = 260.0;

const TWO_PI: f32 = 2.0 * core::f32::consts::PI;

bind_interrupts!(
    struct Irqs {
        ADC_IRQ_FIFO => InterruptHandler;
    }
);

#[embassy_executor::main]
async fn main(_spawner: Spawner) {
    let p = embassy_rp::init(Default::default());

    let mut drv_nsleep = Output::new(p.PIN_5, Level::Low);

    let mut adc = Adc::new(p.ADC, Irqs, AdcConfig::default());
    let mut mascon = Channel::new_pin(p.PIN_26, Pull::None);

    let mut pwm_uv_cfg = default_pwm_config();
    let mut pwm_w_cfg = pwm_uv_cfg.clone();

    let mut pwm_uv = Pwm::new_output_ab(p.PWM_SLICE1, p.PIN_2, p.PIN_3, pwm_uv_cfg.clone());
    let mut pwm_w = Pwm::new_output_a(p.PWM_SLICE2, p.PIN_4, pwm_w_cfg.clone());

    let mut command_filtered = 0.0f32;
    let mut elec_freq_hz = 0.0f32;
    let mut elec_theta = 0.0f32;
    let mut vvvf_clock_s = 0.0f32;

    let mut ticker = Ticker::every(Duration::from_micros(CONTROL_PERIOD_US));

    loop {
        ticker.next().await;

        let command = mascon_read_norm(adc.read(&mut mascon).await.unwrap_or(0));
        command_filtered += (command - command_filtered) * 0.04;

        let target_freq = command_filtered * MAX_ELEC_FREQ_HZ;
        let max_step = FREQ_SLEW_HZ_PER_S * CONTROL_PERIOD_S;
        let df = clampf(target_freq - elec_freq_hz, -max_step, max_step);
        elec_freq_hz = clampf(elec_freq_hz + df, 0.0, MAX_ELEC_FREQ_HZ);

        if command_filtered < 0.02 && elec_freq_hz < 0.5 {
            drv_nsleep.set_low();
            pwm_uv_cfg.compare_a = 0;
            pwm_uv_cfg.compare_b = 0;
            pwm_w_cfg.compare_a = 0;
            pwm_uv.set_config(&pwm_uv_cfg);
            pwm_w.set_config(&pwm_w_cfg);
            continue;
        }

        drv_nsleep.set_high();
        vvvf_clock_s += CONTROL_PERIOD_S;

        elec_theta += TWO_PI * elec_freq_hz * CONTROL_PERIOD_S;
        if elec_theta >= TWO_PI {
            elec_theta = fmodf(elec_theta, TWO_PI);
        }

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
    }
}

fn default_pwm_config() -> PwmConfig {
    let mut cfg = PwmConfig::default();
    cfg.divider = 1u8.into();
    cfg.top = pwm_wrap(clk_sys_freq(), PWM_FREQ_HZ, 1.0);
    cfg.compare_a = cfg.top / 2;
    cfg.compare_b = cfg.top / 2;
    cfg
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
    let mut x = raw as f32 / 4095.0;
    if x < 0.03 {
        x = 0.0;
    }
    clampf(x, 0.0, 1.0)
}

fn vvvf_amplitude(throttle: f32, time_s: f32) -> f32 {
    let notch = floorf(throttle * 5.0 + 0.5) / 5.0;
    let vf = 0.18 + 0.78 * throttle;
    let beat = 0.06 * sinf(TWO_PI * (13.0 + 28.0 * throttle) * time_s);
    let growl = 0.03 * sinf(TWO_PI * (2.2 + throttle * 6.0) * time_s);
    clampf(vf + 0.06 * notch + beat + growl, 0.12, 0.95)
}

fn phase_duty(theta: f32, amp: f32, h5: f32, h7: f32, wobble: f32) -> f32 {
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
