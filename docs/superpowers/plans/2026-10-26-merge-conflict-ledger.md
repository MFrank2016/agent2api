# Merge Conflict Ledger — plugin architecture migration

- **Date:** 2026-10-26
- **Branch:** `migrate/plugin-arch`
- **HEAD (pre-merge):** `413b92c4e5676a74aeaf1e252584ab2f606c69b7`
- **Merged ref:** `upstream/beta` = `6f47f7f868466196e9edafc0b4980382cd19e838`
- **Merge command:** `git merge --no-commit --no-ff upstream/beta` (exit code 1, expected)
- **Merge state:** active and uncommitted (ruling R1); `MERGE_HEAD` = `6f47f7f868466196e9edafc0b4980382cd19e838`
- **Total unmerged files:** **369**
- **Conflict list:** `/tmp/merge-conflicts.txt`

## Area counts

| Area | Path | Count |
| --- | --- | ---: |
| server | `desktop-tauri/src-tauri/` | 105 |
| i18n | `desktop-tauri/i18n/` | 76 |
| mobile | `desktop-tauri/mobile/` | 71 |
| ui | `desktop-tauri/ui/` | 68 |
| ui-islands | `desktop-tauri/ui-islands/` | 28 |
| ui-kit | `desktop-tauri/ui-kit/` | 2 |
| desktop-tauri root | `desktop-tauri/package.json` | 1 |
| docs | `docs/` | 12 |
| root | `.gitignore`, `.github/FUNDING.yml`, `.github/workflows/build.yml`, `LICENSE`, `README.md`, `package.json` | 6 |
| **Total** | | **369** |

`desktop-tauri/` subtotal: 351. Root + docs: 18.

## LICENSE

`LICENSE` is a content conflict and is **intentionally left unresolved**. License
decision **A (AGPL-3.0-only)** applies; resolution is deferred to a later task.
Do not `git checkout --ours/--theirs` or otherwise auto-resolve it.

## Rules in effect

- Ruling **R1**: leave the merge uncommitted. Later tasks resolve conflicts.
- No `git merge --abort`, `git commit`, `git add`, `git reset`, or `checkout --ours/--theirs`
  was run as part of this task.
- Mechanical conflicts may be handled later; semantic conflicts are deferred to
  Phase 2/3/4 of the migration plan.

## Local mobile changes to replay in Task 4.4

Generated with `git diff --name-status $(git merge-base main upstream/beta)..main -- desktop-tauri/mobile` (merge-base `29322eba5511710de72e9bb963eeeb292ab95e58`). All entries are `A`: the mobile app was added on `main` after the merge base. These paths are **not** resolved or staged by Task 1.2; replay them in Phase 4.4.

```text
A	desktop-tauri/mobile/android/.gitignore
A	desktop-tauri/mobile/android/app/.gitignore
A	desktop-tauri/mobile/android/app/build.gradle
A	desktop-tauri/mobile/android/app/capacitor.build.gradle
A	desktop-tauri/mobile/android/app/proguard-rules.pro
A	desktop-tauri/mobile/android/app/src/androidTest/java/com/getcapacitor/myapp/ExampleInstrumentedTest.java
A	desktop-tauri/mobile/android/app/src/main/AndroidManifest.xml
A	desktop-tauri/mobile/android/app/src/main/java/com/agent2api/mobile/MainActivity.java
A	desktop-tauri/mobile/android/app/src/main/res/drawable-land-hdpi/splash.png
A	desktop-tauri/mobile/android/app/src/main/res/drawable-land-mdpi/splash.png
A	desktop-tauri/mobile/android/app/src/main/res/drawable-land-xhdpi/splash.png
A	desktop-tauri/mobile/android/app/src/main/res/drawable-land-xxhdpi/splash.png
A	desktop-tauri/mobile/android/app/src/main/res/drawable-land-xxxhdpi/splash.png
A	desktop-tauri/mobile/android/app/src/main/res/drawable-port-hdpi/splash.png
A	desktop-tauri/mobile/android/app/src/main/res/drawable-port-mdpi/splash.png
A	desktop-tauri/mobile/android/app/src/main/res/drawable-port-xhdpi/splash.png
A	desktop-tauri/mobile/android/app/src/main/res/drawable-port-xxhdpi/splash.png
A	desktop-tauri/mobile/android/app/src/main/res/drawable-port-xxxhdpi/splash.png
A	desktop-tauri/mobile/android/app/src/main/res/drawable-v24/ic_launcher_foreground.xml
A	desktop-tauri/mobile/android/app/src/main/res/drawable/ic_launcher_background.xml
A	desktop-tauri/mobile/android/app/src/main/res/drawable/splash.png
A	desktop-tauri/mobile/android/app/src/main/res/layout/activity_main.xml
A	desktop-tauri/mobile/android/app/src/main/res/mipmap-anydpi-v26/ic_launcher.xml
A	desktop-tauri/mobile/android/app/src/main/res/mipmap-anydpi-v26/ic_launcher_round.xml
A	desktop-tauri/mobile/android/app/src/main/res/mipmap-hdpi/ic_launcher.png
A	desktop-tauri/mobile/android/app/src/main/res/mipmap-hdpi/ic_launcher_foreground.png
A	desktop-tauri/mobile/android/app/src/main/res/mipmap-hdpi/ic_launcher_round.png
A	desktop-tauri/mobile/android/app/src/main/res/mipmap-mdpi/ic_launcher.png
A	desktop-tauri/mobile/android/app/src/main/res/mipmap-mdpi/ic_launcher_foreground.png
A	desktop-tauri/mobile/android/app/src/main/res/mipmap-mdpi/ic_launcher_round.png
A	desktop-tauri/mobile/android/app/src/main/res/mipmap-xhdpi/ic_launcher.png
A	desktop-tauri/mobile/android/app/src/main/res/mipmap-xhdpi/ic_launcher_foreground.png
A	desktop-tauri/mobile/android/app/src/main/res/mipmap-xhdpi/ic_launcher_round.png
A	desktop-tauri/mobile/android/app/src/main/res/mipmap-xxhdpi/ic_launcher.png
A	desktop-tauri/mobile/android/app/src/main/res/mipmap-xxhdpi/ic_launcher_foreground.png
A	desktop-tauri/mobile/android/app/src/main/res/mipmap-xxhdpi/ic_launcher_round.png
A	desktop-tauri/mobile/android/app/src/main/res/mipmap-xxxhdpi/ic_launcher.png
A	desktop-tauri/mobile/android/app/src/main/res/mipmap-xxxhdpi/ic_launcher_foreground.png
A	desktop-tauri/mobile/android/app/src/main/res/mipmap-xxxhdpi/ic_launcher_round.png
A	desktop-tauri/mobile/android/app/src/main/res/values/ic_launcher_background.xml
A	desktop-tauri/mobile/android/app/src/main/res/values/strings.xml
A	desktop-tauri/mobile/android/app/src/main/res/values/styles.xml
A	desktop-tauri/mobile/android/app/src/main/res/xml/file_paths.xml
A	desktop-tauri/mobile/android/app/src/main/res/xml/network_security_config.xml
A	desktop-tauri/mobile/android/app/src/test/java/com/getcapacitor/myapp/ExampleUnitTest.java
A	desktop-tauri/mobile/android/build.gradle
A	desktop-tauri/mobile/android/capacitor.settings.gradle
A	desktop-tauri/mobile/android/gradle.properties
A	desktop-tauri/mobile/android/gradle/wrapper/gradle-wrapper.jar
A	desktop-tauri/mobile/android/gradle/wrapper/gradle-wrapper.properties
A	desktop-tauri/mobile/android/gradlew
A	desktop-tauri/mobile/android/gradlew.bat
A	desktop-tauri/mobile/android/keystore.properties.example
A	desktop-tauri/mobile/android/settings.gradle
A	desktop-tauri/mobile/android/variables.gradle
A	desktop-tauri/mobile/apk.html
A	desktop-tauri/mobile/capacitor.config.ts
A	desktop-tauri/mobile/package-lock.json
A	desktop-tauri/mobile/package.json
A	desktop-tauri/mobile/public/manifest.webmanifest
A	desktop-tauri/mobile/public/sw.js
A	desktop-tauri/mobile/src/App.tsx
A	desktop-tauri/mobile/src/api/auth.ts
A	desktop-tauri/mobile/src/api/client.ts
A	desktop-tauri/mobile/src/api/gateway.ts
A	desktop-tauri/mobile/src/components/Icon.tsx
A	desktop-tauri/mobile/src/components/NavBar.tsx
A	desktop-tauri/mobile/src/components/ProviderLogo.tsx
A	desktop-tauri/mobile/src/components/PwaNotice.tsx
A	desktop-tauri/mobile/src/components/Shell.tsx
A	desktop-tauri/mobile/src/components/ui.tsx
A	desktop-tauri/mobile/src/i18n.ts
A	desktop-tauri/mobile/src/lib/format.ts
A	desktop-tauri/mobile/src/lib/providerFacts.ts
A	desktop-tauri/mobile/src/lib/providers.ts
A	desktop-tauri/mobile/src/lib/pwa.ts
A	desktop-tauri/mobile/src/lib/servers.ts
A	desktop-tauri/mobile/src/lib/usePolling.ts
A	desktop-tauri/mobile/src/lib/usePwa.ts
A	desktop-tauri/mobile/src/main.tsx
A	desktop-tauri/mobile/src/pages/AccountSheet.tsx
A	desktop-tauri/mobile/src/pages/AccountsPage.tsx
A	desktop-tauri/mobile/src/pages/AgentPage.tsx
A	desktop-tauri/mobile/src/pages/AssistantPage.tsx
A	desktop-tauri/mobile/src/pages/CheckinPage.tsx
A	desktop-tauri/mobile/src/pages/DocsPage.tsx
A	desktop-tauri/mobile/src/pages/HomePage.tsx
A	desktop-tauri/mobile/src/pages/KeysPage.tsx
A	desktop-tauri/mobile/src/pages/LoginPage.tsx
A	desktop-tauri/mobile/src/pages/LogsPage.tsx
A	desktop-tauri/mobile/src/pages/ModelsPage.tsx
A	desktop-tauri/mobile/src/pages/MorePage.tsx
A	desktop-tauri/mobile/src/pages/ProxiesPage.tsx
A	desktop-tauri/mobile/src/pages/ReportPage.tsx
A	desktop-tauri/mobile/src/pages/RequestsPage.tsx
A	desktop-tauri/mobile/src/pages/ServersPage.tsx
A	desktop-tauri/mobile/src/pages/SettingsPage.tsx
A	desktop-tauri/mobile/src/pages/TasksPage.tsx
A	desktop-tauri/mobile/src/state/store.ts
A	desktop-tauri/mobile/src/styles/mobile.css
A	desktop-tauri/mobile/tsconfig.json
A	desktop-tauri/mobile/vite.config.ts
```

> **Task 1.2 install note:** the original `npm ci` failure was npm 12's
> `allow-remote` policy, not a lockfile/`package.json` mismatch. Rerunning with
> `--registry=https://registry.npmmirror.com` succeeded (`added 189 packages`,
> exit 0), with no lockfile change.
