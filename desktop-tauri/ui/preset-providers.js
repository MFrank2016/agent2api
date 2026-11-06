/* Agent2API · 自定义提供商的预置目录（preset，读服务端目录） */

/**
 * 「添加账号 → API 模板」分段下的预填清单：名称 / 图标 / 默认协议 / Base URL。
 *
 * ── 事实来源在服务端 ────────────────────────────────────────
 * 这张表原先就写在本文件里（24 项静态数组），现在由后端编译期目录
 * `server/core/custom_providers/preset_catalog.rs` 持有，经
 * `GET /api/presets` 出口（`{presets: [...]}`，camelCase）。本文件退化成
 * **读接口的薄壳**：把服务端形状映射回界面沿用的 `PresetRecord` 形状，并保留
 * `list` / `presetOf` / `iconOf` 三个同步读口，消费方（add-provider-pick、
 * add-custom-provider、add-provider-metadata、add-account-modal、models-page）
 * 不必改读取方式。
 *
 * ── 为什么是异步 + 原地填充，而不是同步 XHR ──────────────────
 * 桌面壳里页面由 Tauri 的资源协议（`tauri://localhost`）提供，同源 XHR 打到
 * 的是壳的资源服务器、不是本地网关；网页壳虽然同源，但两条路径统一走
 * `__TAURI_INTERNALS__.invoke('api_request')`（与 providers.js 的
 * `customRequest` 同一条链），而 invoke 只有异步形态。所以这里在脚本加载时
 * 就发起一次请求，把结果**原地**推进 `PRESET_PROVIDERS` 数组（同一个引用），
 * 消费方稍后重画就能看到；弹窗侧另用 `load()` 拿 Promise 补一次重画信号。
 *
 * 目录是编译期常量（不读配置、不落库），一次成功之后不再重拉；失败时列表
 * 留空并 `console.warn`，**不抛错** —— 拿不到目录只是「没有模板卡可点」，
 * 手动新建提供商仍然可用（表单本来就是空白起始）。
 *
 * ── 映射后的字段（与 add-account-bridge.ts 的 PresetRecord 一一对应）────
 *   key              = 服务端 `id`（卡片标记 `preset:<key>`、图标查找键、
 *                      落库的 `presetKey`）
 *   name             展示名（中文原文，与旧表一致，不包 t()）
 *   icon             图标文件名（`assets/providers/` 下）；缺省空串，卡片落回
 *                      首字母徽章（不要拼 `<id>.png`：目录没这个约定，拼错就是碎图）
 *   protocol/baseUrl 取 `protocols[0]`（服务端把两个平铺字段并成了列表，当前
 *                      每家一条；创建时同样取第一条）
 *   hint             Base URL 栏下的备注。**要过 wbI18n.t**：旧表在这里就把
 *                      中文原文包了 t()，词典里也收录了这些句子，漏掉会让非
 *                      简体界面把中文原样印出来
 *   placeholder      自建面板输入框的占位提示（旧表同样不翻译，保持原样）
 *   quirks           上游特判（`urlSuffix` / `headers` / `anthropicToolType`），
 *                      随创建写进提供商记录；取值语义见服务端 preset_catalog
 *   account.noAuth   服务端 `noKey`（「该上游无需鉴权」预勾选）
 *   balance.scheme   服务端 `balance`（余额查询方案；没有该键 = 不查余额）
 *   clientEmulation  服务端顶层字段（OpenCode Zen 的官方客户端伪装）
 *
 * 预置只是**新建自定义提供商表单的预填模板**：选中某一家时把上述值填进表单
 * （都可改），提交仍走 POST /api/custom-providers（服务端另支持 `presetId`
 * 派生，界面当前仍显式带字段），落库后就是一条普通的自定义提供商（custom- 前缀）。
 */
(() => {
  /**
   * 目录的本地副本。**同一个数组实例**贯穿整个页面生命周期：加载完成时原地
   * 填充，消费方手里拿到的引用自动跟上（它们只读不缓存快照）。
   */
  const PRESET_PROVIDERS = [];

  /** 进行中的请求（成功后就一直复用它，目录不会再变） */
  let inflight = null;

  /** 服务端一条 → 界面一条（字段对应关系见文件头）。 */
  function toPreset(item) {
    const first = Array.isArray(item?.protocols) ? item.protocols[0] : null;
    return {
      key: String(item?.id || ''),
      name: String(item?.name || item?.id || ''),
      icon: item?.icon || '',
      protocol: first?.protocol || 'chat_completions',
      baseUrl: first?.baseUrl || '',
      // hint 走词典（旧表在定义时就把中文原文包了 t()，这里保持同一口径）。
      // 这些句子仍留在 i18n-src/*/add-account.json：事实来源搬到服务端后
      // `scan.mjs --check` 会把它们报成「孤儿键」，**别按孤儿删掉** —— 运行时
      // 还是要靠这份译文。
      hint: item?.hint ? (window.wbI18n?.t?.(item.hint) ?? item.hint) : '',
      placeholder: item?.placeholder || '',
      quirks: item?.quirks || {},
      account: { noAuth: item?.noKey === true },
      balance: item?.balance ? { scheme: item.balance } : undefined,
      clientEmulation: item?.clientEmulation,
    };
  }

  /** 吸收一份 `{presets: [...]}`；形状不对的条目直接跳过（不猜）。 */
  function absorb(payload) {
    const items = Array.isArray(payload?.presets) ? payload.presets : [];
    PRESET_PROVIDERS.length = 0;
    for (const item of items) {
      const preset = toPreset(item);
      if (preset.key) PRESET_PROVIDERS.push(preset);
    }
  }

  /**
   * 发一次 `GET /api/presets`。
   *
   * 桌面壳与网页壳都装了 `__TAURI_INTERNALS__.invoke`（两边都是**初始化脚本**，
   * 早于本文件执行），响应已拆掉 `{success, data}` 信封，这里拿到的是
   * `{presets: [...]}`。没有该桥时（例如把 ui/ 当静态站点直接打开）退回同源
   * fetch，两种形态都按同一个 payload 形状收。
   */
  function requestPresets() {
    const internals = window.__TAURI_INTERNALS__;
    if (internals && typeof internals.invoke === 'function') {
      return internals.invoke('api_request', {
        request: { method: 'GET', path: '/api/presets', body: null },
      });
    }
    return fetch('/api/presets', { credentials: 'same-origin' })
      .then(response => {
        if (!response.ok) throw new Error(`HTTP ${response.status}`);
        return response.json();
      })
      .then(body => body?.data ?? body);
  }

  /**
   * 拉一次目录（并发合并成一次）。成功：填充并返回 `list`；失败：保持空列表、
   * 打一条 warn、**允许下次重试**（返回的 Promise 不 reject，调用方 `.then`
   * 直接重画即可）。
   */
  function load() {
    if (inflight) return inflight;
    inflight = Promise.resolve()
      .then(requestPresets)
      .then(payload => {
        absorb(payload);
        // 一次性通知：页面里没有等它的关键路径，留作后续需要重画的入口
        window.dispatchEvent(new CustomEvent('wb-presets-ready'));
        return PRESET_PROVIDERS;
      })
      .catch(error => {
        console.warn('读取预置提供商目录失败:', error?.message || error);
        inflight = null;
        return PRESET_PROVIDERS;
      });
    return inflight;
  }

  /** 按 key 取一家（找不到给 null，调用方自行回落到手动新建） */
  function presetOf(key) {
    return PRESET_PROVIDERS.find(item => item.key === key) || null;
  }

  /** 图标路径：收录过的家出 assets/providers/<file>，没有返回空串（首字母徽章兜底） */
  function iconOf(key) {
    const preset = presetOf(key);
    return preset?.icon ? `assets/providers/${preset.icon}` : '';
  }

  window.wbPresetProviders = { list: PRESET_PROVIDERS, presetOf, iconOf, load };

  // 页面启动即拉一次：用户点开「添加账号」时通常已经到位；没到位的话弹窗的
  // reset 会再调一次 load()（同一个 inflight）拿重画信号。
  load();
})();
