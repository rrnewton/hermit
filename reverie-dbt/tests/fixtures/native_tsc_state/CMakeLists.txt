cmake_minimum_required(VERSION 3.14)
project(native_tsc_state C ASM)
add_executable(native_tsc_state_guest guest.c state.S)
set_property(TARGET native_tsc_state_guest PROPERTY C_STANDARD 11)
target_compile_options(native_tsc_state_guest PRIVATE -Wall -Wextra -Werror -fno-pie)

target_link_options(native_tsc_state_guest PRIVATE -no-pie)
