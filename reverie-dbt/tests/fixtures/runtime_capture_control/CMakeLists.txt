cmake_minimum_required(VERSION 3.14)
project(runtime_capture_control C)

add_executable(runtime_capture_guest guest.c)
set_property(TARGET runtime_capture_guest PROPERTY C_STANDARD 11)
target_compile_options(runtime_capture_guest PRIVATE -Wall -Wextra -Werror)
