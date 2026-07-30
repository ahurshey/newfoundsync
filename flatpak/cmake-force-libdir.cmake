# CMake toolchain file whose only job is to force GNUInstallDirs to use lib/ rather than lib64/.
#
# Why this exists: shiguredo_svt_av1's build script ends with `dst.join("lib")` — it tells rustc to
# look for the static library it just built in <out>/lib, unconditionally. But GNUInstallDirs picks
# lib64 on 64-bit Fedora/RHEL/SUSE and inside the Freedesktop SDK, so CMake installs the archive to
# <out>/lib64 and the link fails with:
#
#     error: could not find native static library `SvtAv1Enc`, perhaps an -L flag is missing?
#
# Setting it here works because a toolchain file is processed before project()/GNUInstallDirs, so the
# cache entry already exists by the time GNUInstallDirs would compute one — and GNUInstallDirs
# respects a value that is already set. cmake-rs picks this file up from the CMAKE_TOOLCHAIN_FILE
# environment variable (see cmake crate src/lib.rs, getenv_target_os("CMAKE_TOOLCHAIN_FILE")).
#
# Delete this the moment upstream probes both directories.
set(CMAKE_INSTALL_LIBDIR "lib" CACHE PATH "libdir forced to lib for shiguredo_svt_av1" FORCE)
