# Granite artifact preparation

This standalone, curator-only Rust tool converts the pinned official Granite R2 ONNX
into generated Rust source and Burnpack weights. Ordinary workspace builds do not
run it, import weights, or download model files.

```sh
cargo test --locked --manifest-path tools/granite-artifact/Cargo.toml
cargo run --locked --manifest-path tools/granite-artifact/Cargo.toml -- \
  build /path/to/model.onnx /path/to/tokenizer.json /path/to/new-output /path/to/licenses
```

The output directory must not exist. Both input digests are checked before it is
created. The license directory must contain `APACHE-2.0.txt`, `GEMMA-TERMS.html`,
and `NOTICE.txt`; their exact bytes are copied and hashed. Supplying these files
is **not** a licensing review or redistribution approval.

Outputs:

- `model.rs`, `model.bpk`, `tokenizer.json`
- `artifact-manifest.json`: full embedding identity and pinned artifact hashes
- `provenance.json`: importer crate/lock hashes, settings, and license-file hashes
- `semantic-tensors.json`: tensor names, dtypes, shapes and payload hashes, plus
  producer metadata and typed scalar descriptions
- `licenses/`: the supplied license/notice packet

A failed build leaves `BUILD_INCOMPLETE` in its new output directory. Do not use
or publish that directory; start a new build in a different directory. The runtime
resolver rejects this marker. The importer stages the physical ONNX file under
`model.onnx` in the new output directory and uses relative input/output names,
so raw generated source does not embed machine-local input paths. Staging uses
a hard link when possible, or a copy requiring about 1.25 GB of transient disk
space across filesystems; the staged input is removed after successful import.
The original input is never modified. Hugging Face cache symlinks are supported.

## Repeat conversion

Run two conversions into separate new directories. Compare generated source,
semantic tensors, and scalar/producer metadata separately from container byte
hashes. Burn 0.22 generates fresh parameter IDs: repeated conversions can have
identical source and tensor payloads but different Burnpack hashes. Do not claim
bit-reproducibility or replace a pinned container hash solely because tensors
compare equal. The runtime trusts the compiled manifest, not a manifest supplied
in a local override directory.

For an existing pack, inspect without running inference:

```sh
cargo run --locked --manifest-path tools/granite-artifact/Cargo.toml -- \
  inspect /path/to/model.bpk
```

## Distribution gate

IBM identifies the weights as Apache-2.0, but its model card's Tokenizer Attribution
identifies the tokenizer as Gemma3-derived and subject to the
[Gemma Terms of Use](https://ai.google.dev/gemma/terms). The bundle is not Apache-only.
Publication requires separate owner approval and tokenizer licensing review;
this tool does not assert a settled compatibility verdict.

The compiled runtime manifest currently has **no download URLs**. A freshly
converted local pack is usable only when its bytes match that compiled manifest;
because container IDs vary, regenerating weights is not a way to reproduce the
pinned pack byte-for-byte. There is no published-install promise or automatic
artifact upload. Retain the verified pack associated with a reviewed manifest.
