## 改动内容

<!-- 简述做了什么 -->

## 动机

<!-- 为什么改 / 解决什么问题 / 关联 issue -->

## 测试

- [ ] `cargo fmt --all --check`
- [ ] `cargo clippy --workspace --all-targets -- -D warnings`
- [ ] `cargo test --workspace`

## 安全自检（若涉及鉴权 / 权限审批 / 凭据 / 沙箱）

- [ ] 未削弱白名单鉴权
- [ ] 未放宽 `allowedTools` / workdir 边界
- [ ] 未引入绕过频率/风控的逻辑
