#include <math.h>
#include <stdio.h>

#include "pico/stdlib.h"
#include "hardware/adc.h"
#include "hardware/clocks.h"
#include "hardware/gpio.h"
#include "hardware/pwm.h"

#ifndef M_PI
#define M_PI 3.14159265358979323846
#endif

static const uint PIN_PHASE_U = 2;
static const uint PIN_PHASE_V = 3;
static const uint PIN_PHASE_W = 4;
static const uint PIN_DRV_NSLEEP = 5;
static const uint PIN_MASCON_ADC = 26;   // ADC0
static const uint ADC_CHANNEL = 0;

static const float PWM_FREQ_HZ = 20000.0f;
static const float CONTROL_PERIOD_S = 0.0001f;  // 10kHz
static const float MAX_ELEC_FREQ_HZ = 380.0f;
static const float FREQ_SLEW_HZ_PER_S = 260.0f;

static uint pwm_slice;
static uint pwm_wrap;
static float command_filtered = 0.0f;
static float elec_freq_hz = 0.0f;
static float elec_theta = 0.0f;
static float vvvf_clock_s = 0.0f;

static inline float clampf(float v, float lo, float hi) {
    if (v < lo) return lo;
    if (v > hi) return hi;
    return v;
}

static void pwm_setup(void) {
    gpio_set_function(PIN_PHASE_U, GPIO_FUNC_PWM);
    gpio_set_function(PIN_PHASE_V, GPIO_FUNC_PWM);
    gpio_set_function(PIN_PHASE_W, GPIO_FUNC_PWM);

    pwm_slice = pwm_gpio_to_slice_num(PIN_PHASE_U);
    uint slice_v = pwm_gpio_to_slice_num(PIN_PHASE_V);
    uint slice_w = pwm_gpio_to_slice_num(PIN_PHASE_W);

    if (slice_v != pwm_slice || slice_w != pwm_slice) {
        while (true) {
            tight_loop_contents();
        }
    }

    float sys_hz = (float)clock_get_hz(clk_sys);
    float divider = 1.0f;
    pwm_wrap = (uint)(sys_hz / (divider * PWM_FREQ_HZ) - 1.0f);
    if (pwm_wrap > 65535u) pwm_wrap = 65535u;
    if (pwm_wrap < 1000u) pwm_wrap = 1000u;

    pwm_config cfg = pwm_get_default_config();
    pwm_config_set_clkdiv(&cfg, divider);
    pwm_config_set_wrap(&cfg, pwm_wrap);
    pwm_init(pwm_slice, &cfg, true);

    pwm_set_gpio_level(PIN_PHASE_U, pwm_wrap / 2u);
    pwm_set_gpio_level(PIN_PHASE_V, pwm_wrap / 2u);
    pwm_set_gpio_level(PIN_PHASE_W, pwm_wrap / 2u);
}

static void adc_setup(void) {
    adc_init();
    adc_gpio_init(PIN_MASCON_ADC);
    adc_select_input(ADC_CHANNEL);
}

static inline float mascon_read_norm(void) {
    uint16_t raw = adc_read();
    float x = (float)raw / 4095.0f;
    if (x < 0.03f) x = 0.0f;
    return clampf(x, 0.0f, 1.0f);
}

static inline float vvvf_amplitude(float throttle, float time_s) {
    float notch = floorf(throttle * 5.0f + 0.5f) / 5.0f;
    float vf = 0.18f + 0.78f * throttle;
    float beat = 0.06f * sinf(2.0f * (float)M_PI * (13.0f + 28.0f * throttle) * time_s);
    float growl = 0.03f * sinf(2.0f * (float)M_PI * (2.2f + throttle * 6.0f) * time_s);
    return clampf(vf + 0.06f * notch + beat + growl, 0.12f, 0.95f);
}

static bool control_cb(struct repeating_timer *t) {
    (void)t;

    const float command = mascon_read_norm();
    command_filtered += (command - command_filtered) * 0.04f;

    const float target_freq = command_filtered * MAX_ELEC_FREQ_HZ;
    const float max_step = FREQ_SLEW_HZ_PER_S * CONTROL_PERIOD_S;
    float df = target_freq - elec_freq_hz;
    df = clampf(df, -max_step, max_step);
    elec_freq_hz += df;
    elec_freq_hz = clampf(elec_freq_hz, 0.0f, MAX_ELEC_FREQ_HZ);

    if (command_filtered < 0.02f && elec_freq_hz < 0.5f) {
        gpio_put(PIN_DRV_NSLEEP, 0);
        pwm_set_gpio_level(PIN_PHASE_U, 0);
        pwm_set_gpio_level(PIN_PHASE_V, 0);
        pwm_set_gpio_level(PIN_PHASE_W, 0);
        return true;
    }

    gpio_put(PIN_DRV_NSLEEP, 1);
    vvvf_clock_s += CONTROL_PERIOD_S;

    elec_theta += 2.0f * (float)M_PI * elec_freq_hz * CONTROL_PERIOD_S;
    if (elec_theta >= 2.0f * (float)M_PI) {
        elec_theta = fmodf(elec_theta, 2.0f * (float)M_PI);
    }

    const float amp = vvvf_amplitude(command_filtered, vvvf_clock_s);
    const float h5 = 0.11f * command_filtered;
    const float h7 = 0.07f * command_filtered;
    const float wobble = 0.04f * sinf(2.0f * (float)M_PI * 31.0f * vvvf_clock_s);

    const float offsets[3] = {0.0f, -2.0f * (float)M_PI / 3.0f, 2.0f * (float)M_PI / 3.0f};
    const uint pins[3] = {PIN_PHASE_U, PIN_PHASE_V, PIN_PHASE_W};

    for (int i = 0; i < 3; ++i) {
        const float th = elec_theta + offsets[i];
        float phase_wave = sinf(th) + h5 * sinf(5.0f * th) + h7 * sinf(7.0f * th);
        phase_wave /= 1.18f;
        float duty = 0.5f + 0.5f * amp * (phase_wave + wobble);
        duty = clampf(duty, 0.03f, 0.97f);
        pwm_set_gpio_level(pins[i], (uint16_t)(duty * (float)pwm_wrap));
    }

    return true;
}

int main(void) {
    stdio_init_all();
    sleep_ms(50);

    gpio_init(PIN_DRV_NSLEEP);
    gpio_set_dir(PIN_DRV_NSLEEP, GPIO_OUT);
    gpio_put(PIN_DRV_NSLEEP, 0);

    adc_setup();
    pwm_setup();

    struct repeating_timer timer;
    if (!add_repeating_timer_us(-(int)(CONTROL_PERIOD_S * 1000000.0f), control_cb, NULL, &timer)) {
        while (true) {
            gpio_put(PIN_DRV_NSLEEP, 0);
            sleep_ms(250);
        }
    }

    while (true) {
        sleep_ms(100);
    }
}
