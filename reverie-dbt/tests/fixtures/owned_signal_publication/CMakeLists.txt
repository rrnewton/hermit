cmake_minimum_required(VERSION 3.14)
project(owned_signal_publication C)

add_executable(owned_signal_publication_guest guest.c)
set_property(TARGET owned_signal_publication_guest PROPERTY C_STANDARD 11)
target_compile_options(owned_signal_publication_guest PRIVATE -Wall -Wextra -Werror)
