# imagent fuzz

cargo-fuzz 模糊测试（CODE_REVIEW_v2 E-7）。覆盖 ilink proto 帧解析 + 媒体 SSRF host 校验的鲁棒性（任意输入不 panic）。

## 前置

需 nightly 工具链 + cargo-fuzz：

```bash
rustup toolchain install nightly
cargo +nightly install cargo-fuzz
```

## 运行

```bash
cd fuzz
cargo +nightly fuzz run ilink_proto_parse
cargo +nightly fuzz run ilink_media_cdn_host
```

corpus / crash artifacts 存于 `fuzz/<target>/`（已 gitignore）。

## target

| target | 覆盖 |
|---|---|
| `ilink_proto_parse` | `proto::parse_frame` 帧解析（任意字节） |
| `ilink_media_cdn_host` | `media::assert_cdn_host` SSRF host 校验（任意 URL） |
