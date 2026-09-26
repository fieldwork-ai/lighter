#!/bin/sh
# The corpus: 256 MiB of words drawn from a fixed list by a seeded generator,
# the same bytes on every target (node is on the Mac and in the image), and
# text-like, so zstd has work to do: quantized model weights, tried first,
# are near enough random that zstd passed them through untouched.
set -eu
node -e '
const fs=require("fs"); const words="the of and to in is that for it as with was on be by at this from or an are not have but which one all were they we when your can said there use each she which do how their if will up other about out many then them these so some her would make like him into time has look two more write go see number no way could people my than first water been call who oil its now find long down day did get come made may part".split(" ");
let s=2463534242, out=[], n=0; const f=fs.openSync(process.argv[1],"w");
while (n < 268435456) { let line=""; for (let i=0;i<12;i++){ s^=s<<13; s>>>=0; s^=s>>>17; s^=s<<5; s>>>=0; line+=words[(s>>>8)%words.length]+" "; } line+="\n"; out.push(line); n+=line.length; if (out.length===4096){ fs.writeSync(f,out.join("")); out=[]; } }
if (out.length) fs.writeSync(f,out.join("")); fs.closeSync(f);' "${TMPDIR:-/tmp}/zstd-corpus"
truncate -s 268435456 "${TMPDIR:-/tmp}/zstd-corpus"
