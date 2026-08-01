cmake_minimum_required(VERSION 3.24)
project(qr_cpp_compiler CXX)

set(CMAKE_CXX_STANDARD 20)
set(CMAKE_CXX_STANDARD_REQUIRED ON)
set(CMAKE_POSITION_INDEPENDENT_CODE ON)
set(CMAKE_INTERPROCEDURAL_OPTIMIZATION OFF)   # keep the comparison about codegen

option(QR_FAST_MATH "Build with fast-math / relaxed FP semantics" OFF)
option(QR_VEC_REPORT "Emit vectorization reports (not for timed builds)" OFF)
option(QR_BUILD_PYTHON "Build the nanobind module" ON)

# --- Per-compiler optimization flags -----------------------------------------
# Highest practical optimization for each compiler, native codegen for the
# benchmarking host. Strict variants keep IEEE semantics; fast variants opt
# into each compiler's relaxed-FP model (which is what unlocks vectorized
# transcendentals: libmvec for gcc/clang, SVML for icpx).
set(QR_OPT_FLAGS "")
if(CMAKE_CXX_COMPILER_ID STREQUAL "IntelLLVM")
    list(APPEND QR_OPT_FLAGS -O3 -xHost)
    # Use icpx's bundled lld: inside a nix devShell the PATH `ld` is nix's
    # wrapper, which injects nix rpaths/interpreter into icpx links and
    # produces binaries that crash at loader init (glibc mismatch).
    add_link_options(-fuse-ld=lld)
    # Self-contained runtime lookup: Intel's libsvml/libimf next to the
    # compiler, and the system libstdc++ (icpx targets the system gcc here) —
    # so the module loads without setvars.sh in the benchmark processes.
    get_filename_component(QR_ICPX_BIN ${CMAKE_CXX_COMPILER} DIRECTORY)
    get_filename_component(QR_ICPX_ROOT ${QR_ICPX_BIN} DIRECTORY)
    add_link_options(-Wl,-rpath,${QR_ICPX_ROOT}/lib)
    if(EXISTS /usr/lib/x86_64-linux-gnu/libstdc++.so.6)
        add_link_options(-Wl,-rpath,/usr/lib/x86_64-linux-gnu)
    endif()
    if(QR_FAST_MATH)
        # -fimf-precision=high keeps SVML calls at <1 ulp so cross-compiler
        # validation tolerances stay meaningful.
        list(APPEND QR_OPT_FLAGS -fp-model=fast -fimf-precision=high)
    else()
        list(APPEND QR_OPT_FLAGS -fp-model=precise)
    endif()
    if(QR_VEC_REPORT)
        list(APPEND QR_OPT_FLAGS -qopt-report=3)
    endif()
elseif(CMAKE_CXX_COMPILER_ID STREQUAL "GNU")
    list(APPEND QR_OPT_FLAGS -O3 -march=native -funroll-loops)
    if(QR_FAST_MATH)
        list(APPEND QR_OPT_FLAGS -ffast-math)
    endif()
    if(QR_VEC_REPORT)
        list(APPEND QR_OPT_FLAGS -fopt-info-vec-optimized)
    endif()
elseif(CMAKE_CXX_COMPILER_ID STREQUAL "Clang")
    list(APPEND QR_OPT_FLAGS -O3 -march=native)
    if(QR_FAST_MATH)
        list(APPEND QR_OPT_FLAGS -ffast-math -fveclib=libmvec)
    endif()
    if(QR_VEC_REPORT)
        list(APPEND QR_OPT_FLAGS -Rpass=loop-vectorize)
    endif()
else()
    message(FATAL_ERROR "Unsupported compiler: ${CMAKE_CXX_COMPILER_ID}")
endif()

# icpx is installed outside nix and deliberately targets the SYSTEM gcc
# toolchain (Ubuntu g++ 13 / libstdc++ 6.0.x) rather than nix's: mixing nix's
# glibc-2.40-linked libstdc++ into a system-glibc binary breaks at load time.
# Same-major libstdc++ across variants is asserted by scripts/build_all.sh;
# each variant runs in its own process, and no C++ runtime code is on the
# timed path. QR_GCC_TOOLCHAIN overrides the probe when set explicitly.
if(CMAKE_CXX_COMPILER_ID STREQUAL "IntelLLVM" AND DEFINED ENV{QR_GCC_TOOLCHAIN})
    list(APPEND QR_OPT_FLAGS --gcc-toolchain=$ENV{QR_GCC_TOOLCHAIN})
    add_link_options(--gcc-toolchain=$ENV{QR_GCC_TOOLCHAIN})
endif()

string(TOLOWER "${CMAKE_CXX_COMPILER_ID}" QR_COMPILER_LC)
if(QR_FAST_MATH)
    set(QR_FASTMATH_INT 1)
else()
    set(QR_FASTMATH_INT 0)
endif()

# --- Targets -----------------------------------------------------------------
add_library(qr_core STATIC
    src/pcap_reader.cpp
    src/implied_vol.cpp
    src/svi_fit.cpp
    src/greeks.cpp)
target_include_directories(qr_core PUBLIC include)
target_compile_options(qr_core PUBLIC ${QR_OPT_FLAGS})
string(JOIN " " QR_FLAGS_JOINED ${QR_OPT_FLAGS})
target_compile_definitions(qr_core PUBLIC
    QR_COMPILER_STR="${CMAKE_CXX_COMPILER_ID} ${CMAKE_CXX_COMPILER_VERSION}"
    QR_FLAGS_STR="${QR_FLAGS_JOINED}"
    QR_FASTMATH=${QR_FASTMATH_INT})

add_executable(qr_cli src/main.cpp)
target_link_libraries(qr_cli PRIVATE qr_core)

if(QR_BUILD_PYTHON)
    find_package(Python 3.10 COMPONENTS Interpreter Development.Module REQUIRED)
    execute_process(
        COMMAND ${Python_EXECUTABLE} -m nanobind --cmake_dir
        OUTPUT_VARIABLE nanobind_ROOT OUTPUT_STRIP_TRAILING_WHITESPACE
        COMMAND_ERROR_IS_FATAL ANY)
    find_package(nanobind CONFIG REQUIRED)
    nanobind_add_module(qr_pipeline python/bindings.cpp)
    target_link_libraries(qr_pipeline PRIVATE qr_core)
endif()
