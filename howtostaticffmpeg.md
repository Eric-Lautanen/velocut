# How to Build a Fully Static Rust + FFmpeg App with Hardware Acceleration
### Windows (MINGW64) · Linux · macOS — The guide that would have saved us 30 hours

This guide walks you through building a Rust application that uses FFmpeg as a **completely static binary** — no unexpected DLL dependencies, no installer, just one file you can copy anywhere — while also supporting **hardware-accelerated encoding and decoding** on NVIDIA, AMD, and Intel GPUs across all three major platforms.

---

## Table of Contents

1. [Hardware Acceleration Concepts](#hardware-acceleration-concepts)
2. [Platform Overview: What HWAccel Is Available Where](#platform-overview)
3. [Windows (MINGW64) — Full Static Build](#windows-build)
   - Prerequisites
   - Environment Setup
   - Hardware Dependency Headers
   - FFmpeg Configure & Build
   - Fix the DLL-vs-Static Archive Problem
   - Rust Project Configuration
   - `ffmpeg-sys-the-third` Fork and `build.rs`
4. [Linux — Static-ish Build with HWAccel](#linux-build)
   - Prerequisites
   - NVIDIA NVENC/NVDEC
   - AMD AMF + VAAPI
   - Intel QSV (libvpl)
   - FFmpeg Configure & Build
5. [macOS — VideoToolbox + Static Libs](#macos-build)
   - Prerequisites
   - Apple Silicon vs Intel
   - FFmpeg Configure & Build
6. [Hardware Codec Reference](#hardware-codec-reference)
7. [Rust Usage Patterns](#rust-usage-patterns)
8. [Troubleshooting](#troubleshooting)
9. [Final Verification](#final-verification)
10. [Summary Checklists](#summary-checklists)

---

## Hardware Acceleration Concepts

Before diving into build steps, it is critical to understand **how hardware acceleration actually works** in FFmpeg. This will save you a lot of confusion.

### Build-time vs. Runtime

Hardware acceleration support in FFmpeg is **header-only at build time**. You do not need a GPU, a CUDA SDK, or AMD drivers installed on your build machine. You only need the vendor's header files (`.h`), which define the API functions FFmpeg will call at runtime.

At runtime, FFmpeg dynamically loads the vendor driver DLL/SO from the system — `nvenc64_*.dll` on Windows, `libcuda.so` on Linux, `AMF-*.dll`, etc. If the user's system doesn't have the GPU or driver installed, FFmpeg falls back to software encoding gracefully (or returns an error, depending on how you call it from Rust).

This means:
- You can compile in NVENC support on a machine with no NVIDIA GPU.
- Your users with no NVIDIA GPU will simply get an error if they try to use `h264_nvenc`. Software fallback (`libx264`) still works fine.
- The static binary stays truly static — no GPU SDK DLLs are bundled.

### The HWAccel Pipeline

A full GPU-accelerated transcode pipeline looks like this:

```
Input file
    → HW Decoder (e.g., h264_cuvid)      — decodes frames on GPU
    → HW Pixel Format (e.g., cuda/d3d11)  — frames stay in GPU memory
    → GPU Filter (e.g., scale_cuda)        — resize on GPU, no CPU round-trip
    → HW Encoder (e.g., h264_nvenc)       — encodes on GPU
    → Output file
```

If you skip the HW decoder or use a filter that doesn't support the HW pixel format, FFmpeg automatically downloads frames to CPU memory and back — this is called a **HW surface copy** and significantly reduces the performance benefit.

### GPU Encoder Quality Trade-offs

GPU encoders are faster but typically produce slightly larger files at the same visual quality:

| Vendor | Encoder API | Decoder API | File size vs libx264 |
|--------|-------------|-------------|---------------------|
| NVIDIA | NVENC | NVDEC / CUVID | +5–10% |
| AMD | AMF | DXVA2 / D3D11 (Windows), VAAPI (Linux) | +10–15% |
| Intel | QSV (via libvpl) | QSV | +10–15% |
| Apple | VideoToolbox | VideoToolbox | Good quality |

---

## Platform Overview

### Windows

| GPU | Encode | Decode | API |
|-----|--------|--------|-----|
| NVIDIA | `h264_nvenc`, `hevc_nvenc`, `av1_nvenc` | `h264_cuvid`, `hevc_cuvid` | NVENC/NVDEC |
| AMD | `h264_amf`, `hevc_amf`, `av1_amf` | via `dxva2` / `d3d11va` | AMF + DirectX |
| Intel | `h264_qsv`, `hevc_qsv`, `av1_qsv` | `h264_qsv`, `hevc_qsv` | QSV via libvpl |

Windows also has `d3d11va` and `dxva2` for hardware-accelerated **decoding** that works regardless of GPU brand.

### Linux

| GPU | Encode | Decode | API |
|-----|--------|--------|-----|
| NVIDIA | `h264_nvenc`, `hevc_nvenc`, `av1_nvenc` | `h264_cuvid`, `hevc_cuvid` | NVENC/NVDEC |
| AMD | `h264_amf`, `hevc_amf` | via `vaapi` | AMF (requires amdgpu-pro) |
| Intel | `h264_qsv`, `hevc_qsv` | `h264_qsv` | QSV via libvpl |
| Any GPU | `h264_vaapi`, `hevc_vaapi` | via `vaapi` | VA-API (open standard) |

VA-API is the most portable Linux GPU API and works with Intel, AMD (via mesa), and NVIDIA (via `nvidia-vaapi-driver`).

### macOS

| GPU | Encode | Decode | API |
|-----|--------|--------|-----|
| Apple Silicon (M1/M2/M3/M4) | `h264_videotoolbox`, `hevc_videotoolbox`, `prores_videotoolbox` | `h264_videotoolbox`, `hevc_videotoolbox` | VideoToolbox |
| AMD (Intel Mac) | `h264_videotoolbox`, `hevc_videotoolbox` | VideoToolbox | VideoToolbox |

On macOS, VideoToolbox is the only hardware acceleration API. NVENC, AMF, and QSV are not available. VideoToolbox is included in the OS and requires no additional drivers.

---

## Windows Build (MINGW64)

### Prerequisites

You need **MSYS2** installed from https://www.msys2.org/ (install to the default `C:\msys64`).

Open the **MINGW64** terminal — specifically MINGW64, not MSYS2, not UCRT64 — and run:

```bash
# Update everything first
pacman -Syu

# Core toolchain
pacman -S mingw-w64-x86_64-rust
pacman -S make
pacman -S mingw-w64-x86_64-nasm
pacman -S mingw-w64-x86_64-gcc
pacman -S mingw-w64-x86_64-pkg-config
pacman -S mingw-w64-x86_64-cmake

# Codec and binding dependencies
pacman -S mingw-w64-x86_64-x264
pacman -S mingw-w64-x86_64-clang
pacman -S git
```

Verify:
```bash
which nasm && nasm --version
which gcc && gcc --version
which rustc && rustc --version
pkg-config --exists x264 && echo "x264 ok"
```

### Environment Setup

Add to `~/.bashrc`:

```bash
export LIBCLANG_PATH="C:/msys64/mingw64/bin"
export BINDGEN_EXTRA_CLANG_ARGS="--target=x86_64-w64-mingw32 -I/mingw64/include"
export FFMPEG_DIR=/c/ffmpeg-velocut
export PKG_CONFIG_PATH="/c/ffmpeg-velocut/lib/pkgconfig:/mingw64/lib/pkgconfig"
export PATH="/mingw64/bin:$PATH"

# For NVIDIA NVENC — only needed if you have CUDA Toolkit installed
# export CUDA_PATH="C:/Program Files/NVIDIA GPU Computing Toolkit/CUDA/v12.6"
# export PATH="$CUDA_PATH/bin:$PATH"
```

Then reload: `source ~/.bashrc`

### Hardware Dependency Headers (Build-time only)

These are **header files only** — no GPU or driver needed on your build machine.

#### NVIDIA NVENC/NVDEC Headers

```bash
cd /c/github
git clone https://github.com/FFmpeg/nv-codec-headers.git
cd nv-codec-headers

# Match the version to your FFmpeg version:
# FFmpeg 7.x → use nv-codec-headers n12.2.72.0 or later
# FFmpeg 8.x → use the latest nv-codec-headers
git checkout n12.2.72.0   # or: git log --oneline to find compatible tag

make PREFIX=/mingw64 install
```

This installs headers under `/mingw64/include/ffnvcodec/`. The `--enable-nvenc` and `--enable-cuvid` configure flags will detect them automatically.

> **Note:** You do NOT need the CUDA Toolkit to compile NVENC support. The nv-codec-headers only define the API; the actual NVENC engine is in the GPU driver, loaded at runtime.
>
> If you DO want CUDA-based filters (`scale_npp`, `yadif_cuda`), you'll need to install the [CUDA Toolkit](https://developer.nvidia.com/cuda-downloads) and add `--enable-cuda-nvcc` to the FFmpeg configure command.

#### AMD AMF Headers

```bash
cd /c/github
git clone https://github.com/GPUOpen-LibrariesAndSDKs/AMF.git
cd AMF

# Copy headers to the MINGW64 include path
mkdir -p /mingw64/include/AMF
cp -r amf/public/include/* /mingw64/include/AMF/
```

That's all — AMF is header-only. At runtime, FFmpeg loads `amfrt64.dll` from the AMD driver installation. If the driver isn't present, AMF initialization simply fails and FFmpeg returns an error.

#### Intel QSV Headers (libvpl)

libvpl is the modern successor to the deprecated Intel Media SDK (libmfx). For FFmpeg 6.x and later, prefer libvpl.

```bash
pacman -S mingw-w64-x86_64-libvpl
```

Verify: `pkg-config --exists vpl && echo "libvpl ok"`

If not available via pacman, build from source:
```bash
cd /c/github
git clone https://github.com/intel/libvpl.git
cd libvpl
cmake -B build -DCMAKE_INSTALL_PREFIX=/mingw64 -DBUILD_SHARED_LIBS=OFF
cmake --build build --config Release
cmake --install build
```

### Clone and Build FFmpeg From Source

The MINGW64 system FFmpeg package is built with shared libraries and pulls in a cascade of DLL dependencies. You must build your own.

```bash
cd /c/github
git clone --depth=1 -b n8.0.1 https://github.com/FFmpeg/FFmpeg.git ffmpeg-src
cd ffmpeg-src
```

> Use `n8.0.1` for FFmpeg 8.x. Match this to what your `ffmpeg-sys-the-third` crate expects.

#### Configure with Full Hardware Acceleration

This configure command builds a clean static FFmpeg for a video editor with GPU acceleration support on all three major Windows GPU vendors:

```bash
./configure \
  --prefix=/c/ffmpeg-velocut \
  --target-os=mingw32 \
  --arch=x86_64 \
  --disable-shared \
  --enable-static \
  --disable-programs \
  --disable-doc \
  --disable-network \
  --disable-everything \
  --disable-bzlib \
  --disable-iconv \
  \
  --enable-avcodec \
  --enable-avformat \
  --enable-avfilter \
  --enable-avdevice \
  --enable-swscale \
  --enable-swresample \
  --enable-avutil \
  \
  --enable-protocol=file \
  --enable-demuxer=mov,mp4,matroska,avi,flv,ogg,wav,mp3,aac,webm,asf,mpeg,image2,mpegts \
  --enable-muxer=mp4,matroska,webm,image2 \
  --enable-decoder=h264,hevc,vp8,vp9,av1,mpeg2video,mpeg4,mjpeg,prores,dnxhd,wmv1,wmv2,theora,vorbis,opus,aac,mp3,flac,pcm_s16le,pcm_s24le,pcm_s32le,pcm_f32le,ac3,eac3,dts,wmav2,png \
  --enable-encoder=libx264,aac,png \
  --enable-filter=scale,format,aformat,concat,atrim,trim,setpts,asetpts,fps,blend,volume,amix,aresample,color,overlay,pad,crop,rotate \
  --enable-bsf=h264_mp4toannexb,aac_adtstoasc,hevc_mp4toannexb \
  --enable-parser=h264,hevc,aac,mp3,vp8,vp9,av1,ac3,mpeg4video,opus,vorbis,png \
  --enable-libx264 \
  --enable-gpl \
  \
  --enable-nvenc \
  --enable-nvdec \
  --enable-cuvid \
  --enable-encoder=h264_nvenc,hevc_nvenc,av1_nvenc \
  --enable-decoder=h264_cuvid,hevc_cuvid,vp8_cuvid,vp9_cuvid,av1_cuvid \
  \
  --enable-amf \
  --enable-encoder=h264_amf,hevc_amf,av1_amf \
  --enable-d3d11va \
  --enable-dxva2 \
  \
  --enable-libvpl \
  --enable-encoder=h264_qsv,hevc_qsv,av1_qsv \
  --enable-decoder=h264_qsv,hevc_qsv \
  --enable-filter=scale_qsv,vpp_qsv \
  \
  --extra-cflags="-I/mingw64/include" \
  --extra-ldflags="-L/mingw64/lib -static" \
  --pkg-config-flags="--static"
```

**Hardware acceleration flags explained:**

`--enable-nvenc` and `--enable-nvdec` — NVIDIA encoder/decoder support. Uses headers from `nv-codec-headers`. At runtime, loads `nvenc64_*.dll` from the NVIDIA driver.

`--enable-cuvid` — CUVID (CUDA Video Decoder) for GPU-accelerated decoding via `*_cuvid` decoders. Similar to NVDEC but slightly different API path.

`--enable-amf` — AMD Advanced Media Framework. Uses headers from `AMF/include/`. At runtime, loads `amfrt64.dll` from the AMD Adrenalin driver.

`--enable-d3d11va` and `--enable-dxva2` — DirectX-based hardware decoding. Works with any DirectX-capable GPU (NVIDIA, AMD, Intel). Required for AMD hardware decoding in FFmpeg since AMF currently only handles encoding; decoding goes through DirectX.

`--enable-libvpl` — Intel QSV (Quick Sync Video) via the oneVPL dispatcher. Used for `*_qsv` encoders and decoders. At runtime, loads Intel's media driver.

> **Important:** If you previously built with `--disable-vaapi`, `--disable-dxva2`, and `--disable-d3d11va` for a software-only build, you must re-run configure and `make -j$(nproc) && make install` to include these. There is no shortcut.

#### Build and Install

```bash
make -j$(nproc) && make install
```

5–10 minutes. Compiler warnings are normal.

Verify:
```bash
ls /c/ffmpeg-velocut/lib/
# Should show libavcodec.a, libavfilter.a, etc.

# Verify hwaccel headers are in place
ls /mingw64/include/ffnvcodec/  # NVIDIA
ls /mingw64/include/AMF/core/   # AMD
pkg-config --exists vpl          # Intel
```

### Fix the DLL-vs-Static Archive Problem

MINGW64 ships both `.a` (static archive) and `.dll.a` (import stub) for many libraries. The GNU linker prefers `.dll.a`, meaning your exe ends up requiring the DLL at runtime even though you wanted static linking. Rename the stubs out of the way:

```bash
# Required for true static linking
mv /mingw64/lib/libx264.dll.a /mingw64/lib/libx264.dll.a.bak
mv /mingw64/lib/libz.dll.a /mingw64/lib/libz.dll.a.bak

# If linking libvpl statically
mv /mingw64/lib/libvpl.dll.a /mingw64/lib/libvpl.dll.a.bak 2>/dev/null || true
```

> **After pacman updates:** These files may come back. Re-check if you see `zlib1.dll` or `libx264-165.dll` errors.

### Rust Project Configuration

#### Cargo.toml — workspace root

LTO causes a `Can't find section .llvmbc` linker error with this toolchain:

```toml
[profile.release]
opt-level     = 3
lto           = false
codegen-units = 1
strip         = true
```

#### .cargo/config.toml

```toml
[target.x86_64-pc-windows-gnu]
rustflags = [
  "-L", "/mingw64/lib",
  "-L", "/mingw64/lib/gcc/x86_64-w64-mingw32/15.2.0",
]
```

> **Note:** These `-L` flags don't reliably help with library resolution (GNU ld processes them before undefined references exist). They are belt-and-suspenders. The real search paths come from `build.rs`.

### `ffmpeg-sys-the-third` Fork and `build.rs`

Fork `ffmpeg-sys-the-third` and replace `link_to_libraries()` in `build.rs`:

```rust
fn link_to_libraries(statik: bool) {
    let ffmpeg_ty = if statik { "static" } else { "dylib" };
    for lib in LIBRARIES.iter().filter(|lib| lib.enabled()) {
        println!("cargo:rustc-link-lib={}={}", ffmpeg_ty, lib.name);
    }
    if cargo_feature_enabled("build_zlib") && cfg!(target_os = "linux") {
        println!("cargo:rustc-link-lib=z");
    }
    if statik {
        // Tell the linker where to find MINGW64 libs
        println!("cargo:rustc-link-search=native=/mingw64/lib");

        // Software encoder — x264
        // NOTE: /mingw64/lib/libx264.dll.a must be renamed to libx264.dll.a.bak
        println!("cargo:rustc-link-lib=x264");

        // zlib — PNG codec and some container formats
        // NOTE: /mingw64/lib/libz.dll.a must be renamed to libz.dll.a.bak
        println!("cargo:rustc-link-lib=z");

        // Intel QSV — libvpl (only if you built with --enable-libvpl)
        // NOTE: /mingw64/lib/libvpl.dll.a must be renamed if present
        println!("cargo:rustc-link-lib=vpl");

        // Windows system libraries (required by FFmpeg + HWAccel APIs)
        println!("cargo:rustc-link-lib=bcrypt");
        println!("cargo:rustc-link-lib=ole32");
        println!("cargo:rustc-link-lib=user32");
        println!("cargo:rustc-link-lib=gdi32");

        // d3d11 and dxgi are needed if --enable-d3d11va is set
        // These are Windows system libs; they don't add DLL dependencies beyond
        // what Windows already provides.
        println!("cargo:rustc-link-lib=d3d11");
        println!("cargo:rustc-link-lib=dxgi");

        // C++ runtime — auto-locate via GCC
        let gcc_lib_dir = std::process::Command::new("gcc")
            .args(&["--print-file-name=libgcc_eh.a"])
            .output()
            .map(|o| {
                let path = String::from_utf8_lossy(&o.stdout).trim().to_string();
                std::path::PathBuf::from(path)
                    .parent()
                    .map(|p| p.to_path_buf())
            })
            .ok()
            .flatten();
        if let Some(dir) = gcc_lib_dir {
            println!("cargo:rustc-link-search=native={}", dir.display());
        }
        println!("cargo:rustc-link-lib=stdc++");
        println!("cargo:rustc-link-lib=gcc_eh");
    }
}
```

After pushing any fork changes: `cargo update -p ffmpeg-sys-the-third`

> **Critical:** `cargo clean` does NOT clear the git source cache. Only `cargo update` does.

### Build

```bash
cd /c/github/your-project
cargo build --release
```

---

## Linux Build

### A note on "fully static" on Linux

On Linux, true fully-static builds (against musl or with `--enable-static-link-glibc`) are complex and have edge cases (glibc's `NSS` system does not support static linking safely). For most production use cases, the recommended approach is:

- Build FFmpeg static libraries (`.a`) from source.
- Link your Rust binary against them statically.
- Accept a dependency on system-provided `libc.so`, `libm.so`, `libpthread.so`, and GPU driver shared libraries.

The result is a portable binary that works on any Linux distribution with a compatible glibc version (typically glibc ≥ 2.17 covers all modern distros), and picks up GPU acceleration from whatever driver is installed on the user's machine.

### Prerequisites (Ubuntu/Debian)

```bash
sudo apt update
sudo apt install -y \
  build-essential nasm yasm git cmake pkg-config \
  libx264-dev \
  clang libclang-dev \
  curl
```

Install Rust via rustup if not already installed:
```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
```

### NVIDIA NVENC/NVDEC Headers (Linux)

As on Windows, you only need the headers — no GPU or CUDA SDK required to compile.

```bash
cd ~/src
git clone https://github.com/FFmpeg/nv-codec-headers.git
cd nv-codec-headers
git checkout n12.2.72.0  # match to your FFmpeg version
sudo make install          # installs to /usr/local/include/ffnvcodec/
```

If you also want CUDA-based filters (`scale_npp`, `yadif_cuda`, etc.), install the CUDA Toolkit:

```bash
# Ubuntu — install via apt (simpler than runfile)
sudo apt install nvidia-cuda-toolkit

# Or download the latest from developer.nvidia.com/cuda-downloads
# After installation, verify:
nvcc --version
```

### AMD AMF Headers (Linux)

```bash
cd ~/src
git clone https://github.com/GPUOpen-LibrariesAndSDKs/AMF.git
sudo mkdir -p /usr/local/include/AMF
sudo cp -r AMF/amf/public/include/* /usr/local/include/AMF/
```

On Linux, AMD AMF encoding requires the `amdgpu-pro` driver package which ships `amfrt64.so`. Standard open-source `amdgpu` / Mesa drivers do **not** include AMF. For open-source AMD workflows, use VA-API instead (see below), which works with Mesa's `radeonsi` driver.

```bash
# Install AMD Pro drivers on Ubuntu
# Download the appropriate package from: https://www.amd.com/en/support
./amdgpu-pro-install --opencl=rocr,legacy

# Verify AMF runtime is present
ls /opt/amdgpu-pro/lib/x86_64-linux-gnu/libamfrt64.so*
```

### VA-API (Linux — AMD and Intel, open-source)

VA-API works with Mesa (Intel and AMD) and doesn't require proprietary driver packages. This is the most portable Linux GPU path.

```bash
sudo apt install -y \
  libva-dev \
  libva-drm2 \
  libdrm-dev

# Intel i965 driver (older hardware, Broadwell/Skylake era)
sudo apt install -y i965-va-driver

# Intel iHD driver (recommended for 8th gen+ / Tiger Lake+)
sudo apt install -y intel-media-va-driver-non-free

# AMD open-source (via Mesa)
sudo apt install -y mesa-va-drivers

# Verify
vainfo 2>&1 | head -20
```

Adding the build user to the `render` group is required on most distros:
```bash
sudo usermod -aG render $USER
# Then log out and back in
```

### Intel QSV via libvpl (Linux)

```bash
# Install libvpl (Intel oneVPL)
sudo apt install -y libvpl-dev

# Or build from source for the latest:
cd ~/src
git clone https://github.com/intel/libvpl.git
cd libvpl
cmake -B build -DCMAKE_BUILD_TYPE=Release -DBUILD_SHARED_LIBS=OFF \
      -DCMAKE_INSTALL_PREFIX=/usr/local
cmake --build build -j$(nproc)
sudo cmake --install build

# Install Intel media driver for runtime
sudo apt install -y intel-media-va-driver-non-free libmfx-gen1.2
```

### FFmpeg Configure & Build (Linux)

```bash
cd ~/src
git clone --depth=1 -b n8.0.1 https://github.com/FFmpeg/FFmpeg.git
cd FFmpeg

./configure \
  --prefix=/opt/ffmpeg-static \
  --disable-shared \
  --enable-static \
  --disable-programs \
  --disable-doc \
  --disable-network \
  --disable-everything \
  \
  --enable-avcodec \
  --enable-avformat \
  --enable-avfilter \
  --enable-avdevice \
  --enable-swscale \
  --enable-swresample \
  --enable-avutil \
  \
  --enable-protocol=file \
  --enable-demuxer=mov,mp4,matroska,avi,flv,ogg,wav,mp3,aac,webm,asf,mpeg,image2,mpegts \
  --enable-muxer=mp4,matroska,webm,image2 \
  --enable-decoder=h264,hevc,vp8,vp9,av1,mpeg2video,mpeg4,mjpeg,prores,dnxhd,vorbis,opus,aac,mp3,flac,pcm_s16le,pcm_s24le,pcm_s32le,pcm_f32le,ac3,eac3,dts,png \
  --enable-encoder=libx264,aac,png \
  --enable-filter=scale,format,aformat,concat,atrim,trim,setpts,asetpts,fps,blend,volume,amix,aresample,color,overlay,pad,crop,rotate \
  --enable-bsf=h264_mp4toannexb,aac_adtstoasc,hevc_mp4toannexb \
  --enable-parser=h264,hevc,aac,mp3,vp8,vp9,av1,ac3,mpeg4video,opus,vorbis,png \
  --enable-libx264 \
  --enable-gpl \
  \
  --enable-nvenc \
  --enable-nvdec \
  --enable-cuvid \
  --enable-encoder=h264_nvenc,hevc_nvenc,av1_nvenc \
  --enable-decoder=h264_cuvid,hevc_cuvid,vp9_cuvid,av1_cuvid \
  \
  --enable-amf \
  --enable-encoder=h264_amf,hevc_amf \
  \
  --enable-vaapi \
  --enable-encoder=h264_vaapi,hevc_vaapi,av1_vaapi \
  --enable-decoder=h264,hevc \
  --enable-filter=scale_vaapi,deinterlace_vaapi,overlay_vaapi \
  \
  --enable-libvpl \
  --enable-encoder=h264_qsv,hevc_qsv,av1_qsv \
  --enable-decoder=h264_qsv,hevc_qsv \
  --enable-filter=scale_qsv,vpp_qsv \
  \
  --extra-cflags="-I/usr/local/include" \
  --extra-ldflags="-L/usr/local/lib" \
  --extra-libs="-lpthread -lm" \
  --pkg-config-flags="--static"

make -j$(nproc)
make install
```

#### Rust Environment Setup (Linux)

Add to `~/.bashrc`:

```bash
export FFMPEG_DIR=/opt/ffmpeg-static
export PKG_CONFIG_PATH="/opt/ffmpeg-static/lib/pkgconfig"
export LIBCLANG_PATH="/usr/lib/llvm-14/lib"  # adjust LLVM version as needed
```

#### build.rs for Linux

In your forked `ffmpeg-sys-the-third`, the Linux static block in `link_to_libraries()`:

```rust
if statik {
    println!("cargo:rustc-link-search=native=/usr/local/lib");
    println!("cargo:rustc-link-search=native=/usr/lib/x86_64-linux-gnu");

    println!("cargo:rustc-link-lib=x264");
    println!("cargo:rustc-link-lib=z");
    println!("cargo:rustc-link-lib=vpl");       // Intel QSV
    println!("cargo:rustc-link-lib=va");         // VA-API
    println!("cargo:rustc-link-lib=va-drm");     // VA-API DRM backend
    println!("cargo:rustc-link-lib=drm");
    println!("cargo:rustc-link-lib=pthread");
    println!("cargo:rustc-link-lib=m");
    println!("cargo:rustc-link-lib=stdc++");
}
```

---

## macOS Build

### A note on static linking on macOS

macOS does not support fully static binaries — Apple's system libraries (`libSystem.dylib`) cannot be statically linked, and Apple doesn't provide `.a` versions of them. What you can achieve is a binary with **no third-party shared library dependencies** — only Apple's own system frameworks. This is the practical equivalent of "static" for distribution purposes.

### Prerequisites

Install Xcode Command Line Tools and Homebrew:

```bash
xcode-select --install

# Install Homebrew if not already installed
/bin/bash -c "$(curl -fsSL https://raw.githubusercontent.com/Homebrew/install/HEAD/install.sh)"

brew install nasm yasm pkg-config cmake git rust
brew install x264
```

For Apple Silicon (M1/M2/M3/M4), Homebrew installs to `/opt/homebrew`. For Intel Macs, it's `/usr/local`. Adjust paths accordingly.

### Apple Silicon vs Intel Mac

On **Apple Silicon**, VideoToolbox uses the dedicated media engine in the SoC. Encoding speed and quality are excellent, and AV1 decode is hardware-accelerated on M3/M4.

On **Intel Macs**, VideoToolbox uses the AMD or Intel GPU depending on the model. HEVC (H.265) encode is hardware-accelerated from Intel 7th gen+ / Kaby Lake.

There is no NVENC or AMF on macOS. VideoToolbox is your only option for hardware acceleration.

### FFmpeg Configure & Build (macOS)

```bash
# Determine Homebrew prefix
BREW_PREFIX=$(brew --prefix)

cd ~/src
git clone --depth=1 -b n8.0.1 https://github.com/FFmpeg/FFmpeg.git
cd FFmpeg

./configure \
  --prefix=/opt/ffmpeg-macos \
  --disable-shared \
  --enable-static \
  --disable-programs \
  --disable-doc \
  --disable-network \
  --disable-everything \
  \
  --enable-avcodec \
  --enable-avformat \
  --enable-avfilter \
  --enable-avdevice \
  --enable-swscale \
  --enable-swresample \
  --enable-avutil \
  \
  --enable-protocol=file \
  --enable-demuxer=mov,mp4,matroska,avi,flv,ogg,wav,mp3,aac,webm,asf,mpeg,image2,mpegts \
  --enable-muxer=mp4,matroska,webm,mov,image2 \
  --enable-decoder=h264,hevc,vp8,vp9,av1,mpeg2video,mpeg4,mjpeg,prores,dnxhd,vorbis,opus,aac,mp3,flac,pcm_s16le,pcm_s24le,pcm_s32le,pcm_f32le,ac3,dts,png \
  --enable-encoder=libx264,aac,png,prores \
  --enable-filter=scale,format,aformat,concat,atrim,trim,setpts,asetpts,fps,blend,volume,amix,aresample,color,overlay,pad,crop,rotate \
  --enable-bsf=h264_mp4toannexb,aac_adtstoasc,hevc_mp4toannexb \
  --enable-parser=h264,hevc,aac,mp3,vp8,vp9,av1,ac3,mpeg4video,opus,vorbis,png \
  --enable-libx264 \
  --enable-gpl \
  \
  --enable-videotoolbox \
  --enable-audiotoolbox \
  --enable-encoder=h264_videotoolbox,hevc_videotoolbox,prores_videotoolbox \
  --enable-decoder=h264_videotoolbox,hevc_videotoolbox \
  \
  --extra-cflags="-I${BREW_PREFIX}/include" \
  --extra-ldflags="-L${BREW_PREFIX}/lib" \
  --pkg-config-flags="--static"

make -j$(sysctl -n hw.logicalcpu)
make install
```

#### Rust Environment Setup (macOS)

```bash
export FFMPEG_DIR=/opt/ffmpeg-macos
export PKG_CONFIG_PATH="/opt/ffmpeg-macos/lib/pkgconfig:${BREW_PREFIX}/lib/pkgconfig"
export LIBCLANG_PATH="$(brew --prefix llvm)/lib"
```

#### build.rs for macOS

```rust
if statik {
    println!("cargo:rustc-link-lib=x264");
    println!("cargo:rustc-link-lib=z");
    println!("cargo:rustc-link-lib=bz2");  // macOS ships bz2
    println!("cargo:rustc-link-lib=iconv"); // macOS ships iconv

    // VideoToolbox and AudioToolbox — Apple frameworks for hardware acceleration
    println!("cargo:rustc-link-lib=framework=VideoToolbox");
    println!("cargo:rustc-link-lib=framework=AudioToolbox");
    println!("cargo:rustc-link-lib=framework=CoreMedia");
    println!("cargo:rustc-link-lib=framework=CoreVideo");
    println!("cargo:rustc-link-lib=framework=CoreFoundation");
    println!("cargo:rustc-link-lib=framework=CoreServices");
    println!("cargo:rustc-link-lib=framework=Security");
    println!("cargo:rustc-link-lib=framework=AVFoundation");
    println!("cargo:rustc-link-lib=c++");  // libc++ on macOS, not libstdc++
}
```

> On macOS, note `libc++` (Clang's runtime) instead of `libstdc++` (GCC's). And `framework=` prefix for Apple frameworks.

---

## Hardware Codec Reference

### NVIDIA — Encoders

| Codec | Encoder | Min GPU | Notes |
|-------|---------|---------|-------|
| H.264 | `h264_nvenc` | Kepler (GTX 600) | Most widely supported |
| H.265/HEVC | `hevc_nvenc` | Maxwell (GTX 900) | Better compression than H.264 |
| AV1 | `av1_nvenc` | Ada Lovelace (RTX 4000) | Best quality; requires driver 522+ |

### NVIDIA — Decoders

| Codec | Decoder | Notes |
|-------|---------|-------|
| H.264 | `h264_cuvid` | All NVENC-capable GPUs |
| H.265/HEVC | `hevc_cuvid` | Maxwell+ |
| VP8 | `vp8_cuvid` | Pascal+ |
| VP9 | `vp9_cuvid` | Pascal+ |
| AV1 | `av1_cuvid` | Ampere (RTX 3000)+ |

### NVIDIA — Recommended FFmpeg Commands

```bash
# Full GPU pipeline: decode + encode on GPU (no CPU round-trips)
ffmpeg -hwaccel cuda -hwaccel_output_format cuda \
  -i input.mp4 \
  -c:v h264_nvenc -preset p5 -tune hq -cq 20 \
  -c:a aac -b:a 192k \
  output.mp4

# High quality H.265 with lookahead
ffmpeg -hwaccel cuda -hwaccel_output_format cuda \
  -i input.mp4 \
  -c:v hevc_nvenc -preset p6 -tune hq -cq 24 \
  -rc-lookahead 20 -temporal-aq 1 -bf 3 \
  -c:a aac -b:a 192k \
  output.mp4

# AV1 encode (RTX 4000+ only)
ffmpeg -hwaccel cuda -hwaccel_output_format cuda \
  -i input.mp4 \
  -c:v av1_nvenc -preset p5 -cq 30 \
  output.mp4

# GPU resize + encode
ffmpeg -hwaccel cuda -hwaccel_output_format cuda \
  -i input.mp4 \
  -vf scale_cuda=1280:720 \
  -c:v h264_nvenc -preset p4 -cq 23 \
  output.mp4
```

NVENC presets: `p1` (fastest) through `p7` (slowest/best quality). For most uses, `p4`–`p6` with `-cq` (constant quality mode) gives the best balance.

### AMD — Encoders

| Codec | Encoder | Min GPU | Notes |
|-------|---------|---------|-------|
| H.264 | `h264_amf` | GCN 1.0 (HD 7000) | Requires Adrenalin driver on Windows |
| H.265/HEVC | `hevc_amf` | Polaris (RX 400) | Windows and Linux (amdgpu-pro) |
| AV1 | `av1_amf` | RDNA 3 (RX 7000) | Windows; requires driver 23.3+ |

### AMD — Recommended FFmpeg Commands

```bash
# Windows: AMD AMF encode with D3D11 decode
ffmpeg -hwaccel d3d11va -hwaccel_output_format d3d11 \
  -i input.mp4 \
  -c:v h264_amf -quality quality -rc cqp -qp_i 20 -qp_p 23 \
  -c:a aac -b:a 192k \
  output.mp4

# HEVC encode
ffmpeg -hwaccel d3d11va -hwaccel_output_format d3d11 \
  -i input.mp4 \
  -c:v hevc_amf -quality quality \
  -c:a aac -b:a 192k \
  output.mp4

# Linux: AMD VA-API (open-source, works with Mesa)
ffmpeg -vaapi_device /dev/dri/renderD128 \
  -i input.mp4 \
  -vf 'format=nv12,hwupload,scale_vaapi=1280:720' \
  -c:v h264_vaapi -qp 20 \
  -c:a aac -b:a 192k \
  output.mp4
```

AMF quality presets: `speed`, `balanced`, `quality`. On Windows, decode acceleration goes through `d3d11va` or `dxva2`; AMF itself is encode-only in FFmpeg.

### Intel — Encoders and Decoders

| Codec | Encoder | Decoder | Notes |
|-------|---------|---------|-------|
| H.264 | `h264_qsv` | `h264_qsv` | Broadwell+ |
| H.265/HEVC | `hevc_qsv` | `hevc_qsv` | Skylake+ |
| AV1 | `av1_qsv` | `av1_qsv` (decode) | Arc GPU / Meteor Lake+ for encode |
| VP9 | — | `vp9_qsv` | Kaby Lake+ |

### Intel — Recommended FFmpeg Commands

```bash
# Full QSV pipeline
ffmpeg -hwaccel qsv -c:v h264_qsv \
  -i input.mp4 \
  -vf scale_qsv=1280:720 \
  -c:v h264_qsv -b:v 5M -look_ahead 1 \
  -c:a aac -b:a 192k \
  output.mp4

# HEVC encode with QSV
ffmpeg -hwaccel qsv \
  -i input.mp4 \
  -c:v hevc_qsv -b:v 3M \
  output.mp4

# VPP (Video Post-Processing) filter for deinterlace + scale
ffmpeg -hwaccel qsv -c:v h264_qsv \
  -i interlaced_input.mp4 \
  -vf 'hwupload=extra_hw_frames=64,vpp_qsv=deinterlace=2:w=1280:h=720' \
  -c:v h264_qsv \
  output.mp4
```

### Apple VideoToolbox

| Codec | Encoder | Decoder | Notes |
|-------|---------|---------|-------|
| H.264 | `h264_videotoolbox` | `h264_videotoolbox` | All Apple HW since 2012 |
| H.265/HEVC | `hevc_videotoolbox` | `hevc_videotoolbox` | A9 / Kaby Lake+, excellent on M-series |
| ProRes 422 | `prores_videotoolbox` | built-in | M1+ only for HW encode |
| ProRes 4444 | `prores_videotoolbox` | built-in | M1+ only for HW encode |
| AV1 | — (encode via libaom SW) | `av1_videotoolbox` (decode) | HW AV1 decode on M3+ |

### Apple — Recommended FFmpeg Commands

```bash
# H.264 with quality setting (0–100, higher = better)
ffmpeg -i input.mp4 \
  -c:v h264_videotoolbox -q:v 65 \
  -c:a aac -b:a 192k \
  output.mp4

# HEVC at a target bitrate
ffmpeg -i input.mp4 \
  -c:v hevc_videotoolbox -b:v 5M \
  -c:a aac -b:a 192k \
  output.mp4

# ProRes 422 (for editing)
ffmpeg -i input.mp4 \
  -c:v prores_videotoolbox -profile:v 2 \
  output.mov

# ProRes 4444 with alpha
ffmpeg -i input_with_alpha.mov \
  -c:v prores_videotoolbox -profile:v 4 \
  output.mov
```

---

## Rust Usage Patterns

### Detecting Available Hardware Accelerators

From Rust, you can probe which HW accelerators are available at runtime using the `ffmpeg-next` crate:

```rust
use ffmpeg_next as ffmpeg;

fn list_hw_encoders() -> Vec<String> {
    ffmpeg::init().unwrap();
    let mut encoders = Vec::new();
    let hw_names = ["h264_nvenc", "hevc_nvenc", "h264_amf", "hevc_amf",
                    "h264_qsv", "hevc_qsv", "h264_videotoolbox", "hevc_videotoolbox"];
    for name in &hw_names {
        if ffmpeg::encoder::find_by_name(name).is_some() {
            encoders.push(name.to_string());
        }
    }
    encoders
}
```

### Runtime HW Encoder with Software Fallback

```rust
fn get_best_h264_encoder() -> &'static str {
    let candidates = if cfg!(target_os = "macos") {
        vec!["h264_videotoolbox", "libx264"]
    } else if cfg!(target_os = "windows") {
        vec!["h264_nvenc", "h264_amf", "h264_qsv", "libx264"]
    } else {
        // Linux
        vec!["h264_nvenc", "h264_vaapi", "h264_qsv", "libx264"]
    };

    for name in &candidates {
        if ffmpeg_next::encoder::find_by_name(name).is_some() {
            return name;
        }
    }
    "libx264"  // always available as we compiled it in
}
```

### GPU Pixel Format Handling

When using hardware decoders, the decoded frame is in a GPU-specific pixel format. You must handle this when applying filters or passing to encoders:

```rust
// The pixel format returned by h264_cuvid is AV_PIX_FMT_CUDA (205 on most builds)
// The pixel format returned by h264_qsv is AV_PIX_FMT_QSV
// The pixel format returned by h264_videotoolbox is AV_PIX_FMT_VIDEOTOOLBOX

// If you need CPU access to frames, add a hwdownload filter:
// scale_cuda → hwdownload → format=nv12

// If you need to pass HW frames to a SW encoder, you must download:
// h264_cuvid → hwdownload → format=yuv420p → libx264
```

---

## Troubleshooting

### "could not find native static library `x264`"
- Verify `/usr/local/lib/libx264.a` (Linux) or `/mingw64/lib/libx264.a` (Windows) exists.
- Make sure `cargo:rustc-link-search=native=<path>` is emitted from `build.rs`.
- Run `cargo update -p ffmpeg-sys-the-third` after any `build.rs` changes.

### "fatal error: 'ffnvcodec/nvEncodeAPI.h' file not found"
Install `nv-codec-headers` as described in the hardware headers section. The configure step detected them but the headers aren't in the include path. Re-run `make PREFIX=/mingw64 install` (Windows) or `sudo make install` (Linux) from the nv-codec-headers directory.

### "fatal error: '/usr/include/libavdevice/avdevice.h' file not found"
You built FFmpeg without `--enable-avdevice`. The `ffmpeg-sys-the-third` bindgen step always looks for `avdevice.h`. Add `--enable-avdevice` and rebuild FFmpeg.

### "Can't find section .llvmbc"
LTO is enabled. Set `lto = false` in `[profile.release]` in workspace `Cargo.toml`.

### DLL popup on launch: `libva.dll`, `libva_win32.dll`
You re-enabled VA-API or forgot to disable it. If targeting Windows software-only, add `--disable-vaapi` to your configure. If you want hardware support, these DLLs should be on the user's system — they're part of the Intel driver or AMD Mesa Windows build.

### DLL popup on launch: `zlib1.dll`, `libx264-165.dll`
The linker chose the `.dll.a` import stub. Run:
```bash
mv /mingw64/lib/libx264.dll.a /mingw64/lib/libx264.dll.a.bak
mv /mingw64/lib/libz.dll.a /mingw64/lib/libz.dll.a.bak
```
Then rebuild (no FFmpeg rebuild needed).

### NVENC returns error at runtime: "No capable devices found"
The user's GPU is too old (pre-Kepler), or the NVIDIA driver is too old. Minimum driver for NVENC SDK 13.x (used by FFmpeg 8.x nv-codec-headers) is approximately **driver 520+**. Check: `nvidia-smi --query-gpu=driver_version --format=csv,noheader`.

### AMF returns "AMF not supported on this system"
On Windows: the user doesn't have AMD Adrenalin drivers installed, or has very old drivers. AMF requires driver 21.x+. On Linux: the user doesn't have `amdgpu-pro` installed. Suggest VA-API as the open-source alternative.

### QSV returns "Cannot load libmfx" or "Cannot load libvpl"
The user's system lacks the Intel Media driver. On Windows, install Intel Graphics driver from https://www.intel.com/content/www/us/en/download-center/home.html. On Linux, install `intel-media-va-driver-non-free` and ensure `/dev/dri` permissions are correct.

### VideoToolbox returns "Error while opening encoder" on macOS
- On Intel Macs: HEVC encode requires at least Kaby Lake (7th gen Intel). Older models fall back to software.
- Make sure you built FFmpeg with `--enable-videotoolbox`. Check: `grep videotoolbox /opt/ffmpeg-macos/lib/pkgconfig/libavcodec.pc`.

### `cfg!(target_os = "windows")` returns false in build.rs
When building for `x86_64-pc-windows-gnu` natively in MINGW64, `cfg!` macros in build scripts can evaluate unreliably. Use plain `if statik { }` guards instead.

### "undefined reference to `inflate`"
zlib symbols missing. Check that `cargo:rustc-link-lib=z` is emitted, and that `libz.dll.a` is renamed (Windows).

### "undefined reference to `__imp_*` symbols" (Windows)
These `__imp_` prefixed symbols are from import libraries (`.dll.a` files). One of your libraries is linking against a `.dll.a` stub that expected a DLL to be present. Track down which library has a corresponding `.dll.a` that wasn't renamed.

### vainfo shows no VA-API devices (Linux)
Add yourself to the `render` group: `sudo usermod -aG render $USER`. Then log out and back in. Verify `/dev/dri/renderD128` exists.

---

## Final Verification

### Windows

```bash
# Verify no unexpected DLL dependencies
objdump -p target/release/yourapp.exe | grep "DLL Name"
```

Expected DLLs (hardware acceleration version):
- `KERNEL32.dll`, `USER32.dll`, `msvcrt.dll` — always present
- `ADVAPI32.dll`, `WS2_32.dll`, `ntdll.dll`
- `bcrypt.dll`, `ole32.dll`, `gdi32.dll`
- `d3d11.dll`, `dxgi.dll` — if D3D11VA is compiled in; these are Windows OS DLLs
- `opengl32.dll` — if your Rust UI uses OpenGL

GPU acceleration DLLs are loaded at runtime by FFmpeg's HW initializer — they do NOT appear in the import table of your exe. If a user has NVIDIA drivers, `nvenc64_*.dll` will be loaded lazily at encode time. If they don't, the encoder returns an error. This is the correct and expected behavior.

### Linux

```bash
# Check shared library dependencies
ldd target/release/yourapp

# Expected (only system libraries)
# linux-vdso.so.1
# libc.so.6
# libm.so.6
# libpthread.so.0
# libdl.so.2

# GPU libraries (loaded dynamically at runtime, not linked):
# libcuda.so, libva.so, libvpl.so — these will NOT appear in ldd output
```

### macOS

```bash
otool -L target/release/yourapp
```

Expected output (only Apple system dylibs):
```
/usr/lib/libSystem.B.dylib
/System/Library/Frameworks/VideoToolbox.framework/Versions/A/VideoToolbox
/System/Library/Frameworks/CoreMedia.framework/Versions/A/CoreMedia
# etc.
```

---

## Summary Checklists

### Windows

- [ ] MSYS2 MINGW64 installed with gcc, nasm, make, pkg-config, clang, cmake, x264
- [ ] `~/.bashrc` has `LIBCLANG_PATH`, `BINDGEN_EXTRA_CLANG_ARGS`, `FFMPEG_DIR`, `PKG_CONFIG_PATH`
- [ ] `nv-codec-headers` cloned and installed to `/mingw64` (for NVIDIA)
- [ ] `AMF` headers copied to `/mingw64/include/AMF/` (for AMD)
- [ ] `libvpl` installed via pacman or built from source (for Intel QSV)
- [ ] FFmpeg 8.x source cloned and built with the configure command above
- [ ] `/mingw64/lib/libx264.dll.a` renamed to `.bak`
- [ ] `/mingw64/lib/libz.dll.a` renamed to `.bak`
- [ ] `/mingw64/lib/libvpl.dll.a` renamed to `.bak` (if present)
- [ ] `ffmpeg-sys-the-third` forked and `link_to_libraries()` updated
- [ ] `lto = false` in workspace `Cargo.toml`
- [ ] `cargo update -p ffmpeg-sys-the-third` run after any fork changes
- [ ] `cargo build --release` succeeds
- [ ] `objdump -p` shows only expected system DLLs

### Linux

- [ ] Build tools: gcc, nasm, yasm, cmake, pkg-config, clang, libclang-dev
- [ ] x264 dev headers installed
- [ ] `nv-codec-headers` installed for NVIDIA support
- [ ] AMF headers copied to `/usr/local/include/AMF/` for AMD AMF
- [ ] VA-API dev libraries installed (`libva-dev`, appropriate driver)
- [ ] `libvpl-dev` installed for Intel QSV
- [ ] Added to `render` group for VA-API device access
- [ ] FFmpeg built with hardware flags and installed to `/opt/ffmpeg-static`
- [ ] `FFMPEG_DIR`, `PKG_CONFIG_PATH`, `LIBCLANG_PATH` exported
- [ ] `build.rs` includes correct link libs for your platform
- [ ] `cargo build --release` succeeds
- [ ] `ldd` shows no unexpected shared libs

### macOS

- [ ] Xcode Command Line Tools installed
- [ ] Homebrew installed with nasm, pkg-config, cmake, rust, x264
- [ ] FFmpeg built with `--enable-videotoolbox --enable-audiotoolbox`
- [ ] `FFMPEG_DIR`, `PKG_CONFIG_PATH`, `LIBCLANG_PATH` exported
- [ ] `build.rs` links `framework=VideoToolbox`, `framework=CoreMedia`, etc.
- [ ] `build.rs` uses `libc++` not `libstdc++`
- [ ] `cargo build --release` succeeds
- [ ] `otool -L` shows only Apple system frameworks