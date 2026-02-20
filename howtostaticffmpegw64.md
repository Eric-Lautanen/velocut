# How to Build a Fully Static Rust + FFmpeg App on Windows (MINGW64)
### The guide that would have saved us 10 hours

This guide walks you through building a Rust application that uses FFmpeg as a **completely static single `.exe`** on Windows — no DLL dependencies, no installer, just one file you can copy anywhere.

This is hard. The MINGW64 system FFmpeg package is built with shared libraries and pulls in a nightmare of DLL dependencies. The solution is to build FFmpeg from source with only what you need, then carefully configure the Rust build to link against it correctly.

---

## Prerequisites

You need **MSYS2** installed. Get it from https://www.msys2.org/ and install it to the default `C:\msys64`.

After installing MSYS2, open the **MINGW64** terminal (not MSYS2, not UCRT64 — specifically MINGW64) and run:

```bash
# Update everything first
pacman -Syu

# Install the Rust toolchain
pacman -S mingw-w64-x86_64-rust

# Install build tools
pacman -S make
pacman -S mingw-w64-x86_64-nasm
pacman -S mingw-w64-x86_64-gcc
pacman -S mingw-w64-x86_64-pkg-config

# Install x264 (our only external encoder dependency)
pacman -S mingw-w64-x86_64-x264

# Install clang/libclang (needed by bindgen to generate FFmpeg bindings)
pacman -S mingw-w64-x86_64-clang

# Install git if you don't have it
pacman -S git
```

Verify things are working:
```bash
which nasm && nasm --version     # should show nasm version
which gcc && gcc --version       # should show gcc version
which rustc && rustc --version   # should show rustc version
pkg-config --exists x264 && echo "x264 ok"
```

---

## Step 1: Configure Your Environment

Add these exports to your `~/.bashrc` so they persist across terminal sessions:

```bash
# Open .bashrc
nano ~/.bashrc
```

Add these lines at the bottom (replace any existing FFMPEG_DIR or PKG_CONFIG_PATH lines):

```bash
export LIBCLANG_PATH="C:/msys64/mingw64/bin"
export BINDGEN_EXTRA_CLANG_ARGS="--target=x86_64-w64-mingw32 -I/mingw64/include"
export FFMPEG_DIR=/c/ffmpeg-velocut
export PKG_CONFIG_PATH="/c/ffmpeg-velocut/lib/pkgconfig:/mingw64/lib/pkgconfig"
export PATH="/mingw64/bin:$PATH"
```

Then reload:
```bash
source ~/.bashrc
```

**Why these matter:**
- `LIBCLANG_PATH` — tells bindgen where to find clang so it can generate FFmpeg Rust bindings
- `BINDGEN_EXTRA_CLANG_ARGS` — tells clang what target architecture we're generating bindings for
- `FFMPEG_DIR` — tells the `ffmpeg-sys-the-third` build script where our custom FFmpeg lives
- `PKG_CONFIG_PATH` — checks our custom FFmpeg's pkgconfig first, falls back to system MINGW64
- `PATH` — makes sure MINGW64 tools are found first

---

## Step 2: Clone and Build FFmpeg From Source

The MINGW64 system FFmpeg (`pacman -S mingw-w64-x86_64-ffmpeg`) is built with `--enable-shared` and drags in dozens of DLL dependencies (rsvg, caca, openal, GLib, Cairo, Vulkan, libplacebo...). You cannot statically link against it cleanly.

The solution is to build FFmpeg yourself with only what you need.

### Clone FFmpeg source

```bash
cd /c/github   # or wherever you keep source code
git clone --depth=1 -b n8.0.1 https://github.com/FFmpeg/FFmpeg.git ffmpeg-src
cd ffmpeg-src
```

> **Note:** Use `n8.0.1` to match FFmpeg 8.0.x. If your `ffmpeg-sys-the-third` crate targets a different version, use the matching tag (e.g. `n7.1` for FFmpeg 7.1).

### Configure FFmpeg

This is the exact configure command that produces a clean static build suitable for a video editor:

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
  --disable-vaapi \
  --disable-dxva2 \
  --disable-d3d11va \
  --disable-d3d12va \
  --disable-bzlib \
  --disable-iconv \
  --enable-avcodec \
  --enable-avformat \
  --enable-avfilter \
  --enable-avdevice \
  --enable-swscale \
  --enable-swresample \
  --enable-avutil \
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
  --extra-cflags="-I/mingw64/include" \
  --extra-ldflags="-L/mingw64/lib -static" \
  --pkg-config-flags="--static"
```

**Why each disable flag:**
- `--disable-shared / --enable-static` — build `.a` archives, not `.dll`
- `--disable-programs` — don't build `ffmpeg.exe`, `ffprobe.exe` etc., saves time
- `--disable-doc` — don't build documentation
- `--disable-network` — removes network protocol support (we only need local files)
- `--disable-everything` — start from zero, only enable exactly what we list
- `--disable-vaapi/dxva2/d3d11va/d3d12va` — hardware acceleration APIs that pull in `libva.dll` at runtime. Pure software encode/decode doesn't need these
- `--disable-bzlib` — bzip2 support (only used for obscure MKV subtitle compression). Pulls in `libbz2-1.dll`
- `--disable-iconv` — charset conversion (only for MPEG-TS text encoding edge cases). Pulls in `libiconv-2.dll`
- `--enable-avdevice` — MUST be included even if you don't use it. The `ffmpeg-sys-the-third` bindgen step always looks for `avdevice.h` and panics if it's missing

The configure output should end with:
```
License: GPL version 2 or later
```

### Build and install

```bash
make -j$(nproc) && make install
```

This takes 5-10 minutes. You'll see compiler warnings — those are normal, ignore them.

When done, verify:
```bash
ls /c/ffmpeg-velocut/lib/
# Should show: libavcodec.a  libavfilter.a  libavformat.a  libavutil.a  libswresample.a  libswscale.a  pkgconfig
```

---

## Step 3: Fix the DLL-vs-Static Archive Problem

This is the sneaky part. MINGW64 has both `.a` (static) and `.dll.a` (import stub) archives for many libraries. When both exist, the GNU linker prefers the `.dll.a` version — meaning your exe will require the DLL at runtime even though you wanted static linking.

The affected libraries for our setup are `x264` and `z` (zlib). Fix this by renaming the import stubs out of the way:

```bash
mv /mingw64/lib/libx264.dll.a /mingw64/lib/libx264.dll.a.bak
mv /mingw64/lib/libz.dll.a /mingw64/lib/libz.dll.a.bak
```

With the `.dll.a` files gone, the linker will use `libx264.a` and `libz.a` — the true static archives.

> **Note:** If you ever run `pacman -S` to update these packages, the `.dll.a` files may come back. Check and rename again if you get `zlib1.dll` or `libx264-165.dll` errors when running your exe.

---

## Step 4: Configure Your Rust Project

### Cargo.toml — workspace root

Make sure LTO is disabled. LTO with this toolchain causes a `Can't find section .llvmbc` linker error:

```toml
[profile.release]
opt-level     = 3
lto           = false
codegen-units = 1
strip         = true
```

### .cargo/config.toml

Create this file at the root of your workspace:

```toml
[target.x86_64-pc-windows-gnu]
rustflags = [
  "-L", "/mingw64/lib",
  "-L", "/mingw64/lib/gcc/x86_64-w64-mingw32/15.2.0",
]
```

> **Important:** These `-L` flags in `rustflags` do NOT reliably help with library resolution. GNU ld processes them before any undefined references exist, so it discards them. They are kept as a belt-and-suspenders measure but the real search paths must come from `build.rs` (see below).

---

## Step 5: The ffmpeg-sys-the-third Fork and build.rs

The `ffmpeg-sys-the-third` crate is the low-level binding layer. You need to fork it and modify `build.rs` to emit the correct link directives for your custom FFmpeg build.

### Why fork?

The upstream crate's `link_to_libraries()` function doesn't know about your custom FFmpeg or the specific Windows static linking requirements. You need to add a Windows static block that:
1. Adds `/mingw64/lib` to the linker search path (from build.rs, not config.toml — see above)
2. Links x264 and zlib
3. Links required Windows system libs
4. Auto-locates and links the GCC C++ runtime

### The link_to_libraries() function

Find `link_to_libraries()` in `ffmpeg-sys-the-third/build.rs` and replace it with:

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

        // External encoder — x264
        // NOTE: /mingw64/lib/libx264.dll.a must be renamed to libx264.dll.a.bak
        // otherwise the linker picks the DLL import stub over the static archive
        println!("cargo:rustc-link-lib=x264");

        // zlib — used by PNG codec and some container formats
        // NOTE: /mingw64/lib/libz.dll.a must be renamed to libz.dll.a.bak
        println!("cargo:rustc-link-lib=z");

        // Windows system libraries
        println!("cargo:rustc-link-lib=bcrypt");
        println!("cargo:rustc-link-lib=ole32");
        println!("cargo:rustc-link-lib=user32");
        println!("cargo:rustc-link-lib=gdi32");

        // C++ runtime — auto-locate via GCC so we don't hardcode the path
        // libgcc_eh.a is in the GCC internal lib dir, NOT in /mingw64/lib
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

### After pushing changes to your fork

Every time you push a change to your forked crate, you must bust cargo's git cache:

```bash
cargo update -p ffmpeg-sys-the-third
```

> **Critical:** `cargo clean` does NOT clear the git source cache. Only `cargo update` does. If you push a change and the build doesn't reflect it, this is why.

---

## Step 6: Build Your Rust App

```bash
cd /c/github/your-project
cargo build --release
```

The first build takes a while — bindgen generates FFmpeg bindings and compiles everything from scratch. Subsequent builds are incremental and much faster.

---

## Troubleshooting

### "could not find native static library `x264`"
The linker can't find x264. Make sure:
1. `/mingw64/lib/libx264.a` exists: `ls /mingw64/lib/libx264*`
2. `cargo:rustc-link-search=native=/mingw64/lib` is being emitted from `build.rs`
3. You ran `cargo update -p ffmpeg-sys-the-third` after pushing your build.rs change

### "fatal error: '/usr/include/libavdevice/avdevice.h' file not found"
You built FFmpeg without `--enable-avdevice`. Add it back — bindgen always generates avdevice bindings regardless of whether you use it.

### "Can't find section .llvmbc"
LTO is enabled. Set `lto = false` in your `[profile.release]` in workspace Cargo.toml.

### DLL popup on launch: `libva.dll`, `libva_win32.dll`
You built FFmpeg without disabling hardware acceleration. Rebuild with:
```
--disable-vaapi --disable-dxva2 --disable-d3d11va --disable-d3d12va
```

### DLL popup on launch: `libbz2-1.dll`, `libiconv-2.dll`
You built FFmpeg without disabling bzlib/iconv. Rebuild with:
```
--disable-bzlib --disable-iconv
```

### DLL popup on launch: `zlib1.dll`, `libx264-165.dll`
The linker picked the `.dll.a` import stub over the static archive. Run:
```bash
mv /mingw64/lib/libx264.dll.a /mingw64/lib/libx264.dll.a.bak
mv /mingw64/lib/libz.dll.a /mingw64/lib/libz.dll.a.bak
```
Then rebuild your Rust project (no FFmpeg rebuild needed).

### "undefined reference to `inflate`" / zlib symbols missing
FFmpeg's PNG codec and MKV demuxer use zlib. If you see these at link time, `libz.a` is not being linked. Check that `cargo:rustc-link-lib=z` is in your `link_to_libraries()` and that `libz.dll.a` has been renamed away.

### "undefined reference to `x264_*`" symbols
Same issue as above but for x264. Check `libx264.dll.a` is renamed and `cargo:rustc-link-lib=x264` is emitted.

### `cfg!(target_os = "windows")` returns false in build.rs
When building for `x86_64-pc-windows-gnu` natively in MINGW64, `cfg!` macros in build scripts can evaluate unreliably. Use plain `if statik { }` instead of `if statik && cfg!(target_os = "windows") { }`.

### `cargo:rustc-link-lib=static:+verbatim=libsomething.a` causes "cannot find -lilibsomething.a"
The `+verbatim` form tells the linker to use the name literally with `-l:filename` syntax, but ld then still prepends `lib` — resulting in `-llibsomething.a` which doesn't exist. Don't use `+verbatim` for libraries that already have the standard `lib` prefix. Either use plain name, `static=name`, or rename away the `.dll.a` stub.

---

## Key Concepts to Understand

### Why `-L` in config.toml rustflags doesn't work for lib resolution

GNU ld scans libraries as it encounters undefined symbols. The `rustflags` in `config.toml` are placed **before** the rlib object files in the linker invocation. This means ld scans those library paths before any undefined references exist — so it finds nothing to link and discards them.

`cargo:rustc-link-lib` directives from `build.rs` are placed **after** the rlibs, so undefined references already exist when ld gets to them. This is why all lib directives must come from `build.rs`, not `config.toml`.

### Why the MINGW64 system FFmpeg doesn't work for static linking

The system FFmpeg is built with `--enable-shared`. This means:
1. The `.a` files it ships are actually **import stubs** that just reference the DLLs
2. It was compiled against libraries like rsvg, caca, openal that are themselves DLL-only on MINGW64
3. Even if you try to link statically, you'll end up with a cascade of missing DLL dependencies

Building from source with `--disable-shared --enable-static` and `--disable-everything` gives you real static archives with only the dependencies you actually chose.

### The `.a` vs `.dll.a` preference problem

On MINGW64, many packages ship both:
- `libfoo.a` — true static archive
- `libfoo.dll.a` — import stub that loads `foo.dll` at runtime

When the GNU linker sees `-lfoo` and finds both files in the search path, **it prefers `.dll.a`**. The only reliable fix is to rename or remove the `.dll.a` file so the linker has no choice.

---

## Final Verification

After a successful build, verify your exe has no unexpected DLL dependencies:

```bash
# Check what DLLs your exe needs
objdump -p target/release/yourapp.exe | grep "DLL Name"
```

For a truly static build targeting Windows, you should only see standard Windows system DLLs:
- `KERNEL32.dll`
- `USER32.dll` 
- `msvcrt.dll`
- `ADVAPI32.dll`
- `WS2_32.dll`
- `dbghelp.dll`
- `ntdll.dll`
- `bcrypt.dll`
- `opengl32.dll` (if using OpenGL for UI)

If you see anything else — `libva.dll`, `zlib1.dll`, `libx264-165.dll` etc. — one of the steps above wasn't applied correctly.

---

## Summary Checklist

- [ ] MSYS2 MINGW64 installed with gcc, nasm, make, pkg-config, clang, x264
- [ ] `~/.bashrc` has `LIBCLANG_PATH`, `BINDGEN_EXTRA_CLANG_ARGS`, `FFMPEG_DIR`, `PKG_CONFIG_PATH` set
- [ ] FFmpeg 8.0.1 source cloned to `/c/github/ffmpeg-src`
- [ ] FFmpeg built with the configure command above and installed to `/c/ffmpeg-velocut`
- [ ] `/mingw64/lib/libx264.dll.a` renamed to `.bak`
- [ ] `/mingw64/lib/libz.dll.a` renamed to `.bak`
- [ ] `ffmpeg-sys-the-third` forked and `link_to_libraries()` updated
- [ ] `lto = false` in workspace `Cargo.toml`
- [ ] `cargo update -p ffmpeg-sys-the-third` run after any fork changes
- [ ] `cargo build --release` succeeds
- [ ] Exe launches with no DLL popup dialogs