# For aarch64-pc-windows-msvc builds on GitHub's Windows ARM runners, where
# Visual Studio uses its x64-hosted cross compiler: without this, CMake
# reports the host's AMD64 processor and BoringSSL (built by wreq's btls-sys)
# adds x86-64 assembly to an ARM64 build. Release builds point
# CMAKE_TOOLCHAIN_FILE_aarch64_pc_windows_msvc here.
set(CMAKE_SYSTEM_NAME Windows)
set(CMAKE_SYSTEM_PROCESSOR ARM64)
# BoringSSL's ARM64 assembly needs clang; plain C is fine for a copier.
set(OPENSSL_NO_ASM YES CACHE BOOL "" FORCE)
