// 构建期全局常量：vite.config.ts 的 define 注入（值取 package.json version，
// vitest 经 test.define 同样注入）。SW 更新提示文案携带版本号用。
declare const __APP_VERSION__: string;
