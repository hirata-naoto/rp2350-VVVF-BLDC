cmake_minimum_required(VERSION 3.13)

include(pico_sdk_import.cmake)

set(PICO_PLATFORM rp2350-arm-s)
set(PICO_BOARD pico2)

project(rp2350_vvvf_bldc C CXX ASM)

set(CMAKE_C_STANDARD 11)
set(CMAKE_CXX_STANDARD 17)

pico_sdk_init()

add_executable(rp2350_vvvf_bldc
    src/main.c
)

target_link_libraries(rp2350_vvvf_bldc
    pico_stdlib
    hardware_pwm
    hardware_adc
    hardware_clocks
)

pico_enable_stdio_uart(rp2350_vvvf_bldc 0)
pico_enable_stdio_usb(rp2350_vvvf_bldc 1)

pico_add_extra_outputs(rp2350_vvvf_bldc)
