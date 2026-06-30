# imagent

> **Instant messaging, meet your agent.**

A **Rust** gateway that connects instant-messaging platforms to autonomous coding agents. Chat with your agent from WeChat; it does real work — reads files, runs commands, edits code — and replies. **Platform-agnostic and backend-agnostic by design.**

```
WeChat (iLink) ─┐                       ┌─ Claude Code
                ├─→  imagent core  ─→──┤
WeCom (官方 API) ─┘   (auth · routing · └─ (any agent, future)
                       session · store)
```

## Why

You already have a powerful coding agent (Claude Code). You live in chat. `imagent` is the always-on bridge between them — scan a QR code, DM your bot, and drive real coding work from your phone, with **in-chat permission approval** for dangerous actions.

## Features / roadmap

- **Multi-platform** (`trait Platform`): iLink (personal WeChat DM) + WeCom (official API)
- **Multi-backend** (`trait Backend`): Claude Code (CLI now, ACP later); pluggable
- **Persistent sessions**: per-chat `conversationId → agent sessionId` mapping in SQLite; survives restarts; `/new` `/switch` `/sessions` `/compact`
- **In-chat permission approval** (planned): dangerous tool calls routed to chat for your approve/deny
- **Single binary**, low footprint, cross-compiled — runs on a NAS / small box / laptop

> Status: **research & design complete, implementation not started.** See `docs/DESIGN.md`.

## Differentiation

vs. the closest existing project [`feiyun0112/AgentBridge`](https://github.com/feiyun0112/AgentBridge) (a .NET implementation of the same idea):

| | feiyun (`.NET`) | imagent (`Rust`) |
|---|---|---|
| Sender allowlist (auth) | ✗ none | ✓ required |
| Session persistence | memory only | SQLite |
| Session commands (`/new` …) | only `/cc` | ✓ |
| In-chat permission approval | ✗ | ✓ (planned) |
| Platforms | weixin only | iLink + WeCom |
| Deploy | needs runtime | single binary |

## Quick start

> TODO — implementation pending. Planned: `imagent login --platform ilink` (scan QR) → `imagent start`.

## ⚠️ Compliance & disclaimer

`imagent` talks to personal WeChat via **Tencent's official iLink / ClawBot protocol** (documented at [developers.weixin.qq.com](https://developers.weixin.qq.com/doc/aispeech/knowledge/openapi/Clawbotrelated.html), official package [`@tencent-weixin/openclaw-weixin`](https://github.com/Tencent/openclaw-weixin), MIT). It is **not** a reverse-engineered client (unlike iPad-protocol / PC-hook tools, which violate the WeChat ToS).

- **This project is unofficial and not affiliated with Tencent.** Users must comply with the [《微信ClawBot功能使用条款》](https://github.com/hao-ji-xing/openclaw-weixin/blob/main/protocol.md) and the [《腾讯微信软件许可及服务协议》](https://weixin.qq.com/agreement?lang=zh_CN). The author is **not** responsible for any account restrictions or legal issues.
- iLink grants an **independent bot identity**; **DM works reliably, ordinary WeChat groups generally do not.**
- Media encryption uses **AES-128-ECB** — this is **mandated by the protocol**, not a design choice.
- This project implements **no** frequency/anti-detection evasion (ClawBot ToS §4.6 red line). Bind a **secondary WeChat account**, not your primary one.

See `docs/RESEARCH.md` for the full compliance analysis.

## License

MIT OR Apache-2.0, at your option.
