# Third-Party Licenses

ComicCrate includes or links against the third-party components listed below.
Their licenses apply to those components; ComicCrate itself is licensed under
the MIT License (see `LICENSE`).

## UnRAR

ComicCrate uses the `unrar` Rust crate, which builds and links the bundled
UnRAR library. UnRAR is copyright (c) Alexander Roshal and is distributed under the
following license:

```text
UnRAR License

The UnRAR sources may be used in any software to handle RAR archives without
limitations free of charge, but cannot be used to re-create the RAR compression
algorithm, which is proprietary. Distribution of modified UnRAR sources in
separate form or as a part of other software is permitted, provided that it is
clearly stated in the documentation and source comments that the code may not
be used to develop a RAR (WinRAR) compatible archiver.

The UnRAR sources are distributed in the hope that they will be useful, but
WITHOUT ANY WARRANTY; without even the implied warranty of MERCHANTABILITY or
FITNESS FOR A PARTICULAR PURPOSE.
```

The UnRAR license grants the right to use the sources for handling RAR
archives. ComicCrate uses UnRAR only to extract existing CBR/RAR archives; it
does not implement or expose RAR compression.

## libwebp

ComicCrate encodes WebP pages with libwebp, copyright (c) 2010 Google Inc.
libwebp is distributed under the following 3-clause BSD license:

```text
Copyright (c) 2010, Google Inc. All rights reserved.

Redistribution and use in source and binary forms, with or without
modification, are permitted provided that the following conditions are
met:

  * Redistributions of source code must retain the above copyright
    notice, this list of conditions and the following disclaimer.
  * Redistributions in binary form must reproduce the above copyright
    notice, this list of conditions and the following disclaimer in the
    documentation and/or other materials provided with the distribution.
  * Neither the name of Google nor the names of its contributors may be
    used to endorse or promote products derived from this software without
    specific prior written permission.

THIS SOFTWARE IS PROVIDED BY THE COPYRIGHT HOLDERS AND CONTRIBUTORS "AS
IS" AND ANY EXPRESS OR IMPLIED WARRANTIES, INCLUDING, BUT NOT LIMITED TO,
THE IMPLIED WARRANTIES OF MERCHANTABILITY AND FITNESS FOR A PARTICULAR
PURPOSE ARE DISCLAIMED. IN NO EVENT SHALL THE COPYRIGHT HOLDER OR
CONTRIBUTORS BE LIABLE FOR ANY DIRECT, INDIRECT, INCIDENTAL, SPECIAL,
EXEMPLARY, OR CONSEQUENTIAL DAMAGES (INCLUDING, BUT NOT LIMITED TO,
PROCUREMENT OF SUBSTITUTE GOODS OR SERVICES; LOSS OF USE, DATA, OR
PROFITS; OR BUSINESS INTERRUPTION) HOWEVER CAUSED AND ON ANY THEORY OF
LIABILITY, WHETHER IN CONTRACT, STRICT LIABILITY, OR TORT (INCLUDING
NEGLIGENCE OR OTHERWISE) ARISING IN ANY WAY OUT OF THE USE OF THIS
SOFTWARE, EVEN IF ADVISED OF THE POSSIBILITY OF SUCH DAMAGE.
```

## Other Rust dependencies

The remaining Rust dependencies are listed in `Cargo.lock`. Each crate retains
its own license, and their license texts are available from the corresponding
crate packages on <https://crates.io>.
