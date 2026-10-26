<p align="center">
  <img src="assets/scpto.svg" alt="scpto - 雙窗格終端機檔案傳輸" width="480">
</p>

[English](README.md)

在 macOS 和 Linux 終端機中取代 FileZilla。左側瀏覽本機檔案，右側瀏覽遠端主機，再用鍵盤或窗格間拖放來複製。兩個窗格都能使用任意端點，包括兩個本機目錄或兩台遠端主機。

## 功能

- 透過系統 `ssh` 使用 SFTP，沿用既有 SSH 別名與 sshto 群組。
- 支援鍵盤導覽、多選、滑鼠傳輸、排序，以及可點擊的按鈕列。
- 傳輸佇列支援暫停、重試、衝突處理，以及部分檔案續傳。
- 支援遠端編輯、語法醒目提示預覽、Markdown、圖片及單向目錄同步。
- 可選用 sudo 瀏覽與傳輸、自動重新連線，以及可設定的 VPN 提示。

## 需求

`PATH` 中須有 OpenSSH 用戶端，遠端主機須提供 SFTP server，終端機須具備互動式輸入與輸出。不需要安裝遠端代理程式。遠端 shell 啟動檔不得在非互動指令執行時向 stdout 輸出內容，否則會破壞 SFTP 資料流。

## 安裝

### Homebrew

```sh
brew install thisishugow/tap/scpto
```

使用 `brew upgrade thisishugow/tap/scpto` 更新。Tap 為 [thisishugow/homebrew-tap](https://github.com/thisishugow/homebrew-tap)。

### 發行版執行檔

從 [Releases](https://github.com/thisishugow/scpto/releases) 下載符合平台的壓縮檔，比對發行版提供的 checksum，解壓縮後將 `scpto` 放到 `PATH` 中，例如 `~/.local/bin`。預先編譯的執行檔不需要 Rust。

| 平台 | Target |
| --- | --- |
| macOS，Intel | `x86_64-apple-darwin` |
| macOS，Apple silicon | `aarch64-apple-darwin` |
| Linux，Intel/AMD 64 位元 | `x86_64-unknown-linux-musl` |
| Linux，ARM64 | `aarch64-unknown-linux-musl` |

在 macOS 使用 `shasum -a 256 <archive>`，在 Linux 使用 `sha256sum <archive>` 計算 checksum。Windows 僅支援透過 WSL 執行，並使用 Linux 執行檔及 WSL 內的 OpenSSH。

### 從原始碼安裝

安裝 stable Rust toolchain 後，在原始碼目錄執行：

```sh
cargo install --locked --path .
```

確認 `~/.cargo/bin` 位於 `PATH` 中。

## 使用方式

```sh
scpto                                  # 本機目前目錄與遠端主機選擇器
scpto my-server                        # 右窗格連線至此 SSH 主機
scpto my-server:/srv/files              # 右窗格開啟遠端絕對路徑
scpto --left ./incoming --right ./outgoing
scpto --left source:/srv/data --right destination:/srv/data
scpto --right '[2001:db8::1]:/srv/files'
scpto --help
scpto --version
```

位置參數 `host[:path]` 一律選擇遠端端點，不可與 `--right` 同時使用。每個窗格選項接受 `--left value` 或 `--left=value` 格式（`--right` 同理），各只能指定一次。

窗格選項中的 `host:/path` 與 `host:` 表示遠端；不含冒號的值表示本機。若第一個冒號之前有斜線，也視為本機路徑，因此本機名稱含冒號時可寫成 `./a:b`。遠端路徑必須是絕對路徑；`host:` 使用記住的目錄或遠端家目錄。本機路徑必須是已存在的目錄，可使用相對路徑或以 `~/` 開頭。`--help`/`-h` 與 `--version`/`-V` 必須單獨使用。無效的 CLI 語法會以代碼 2 結束。

## 快捷鍵

操作使用字母與一般導覽鍵，不需要功能鍵。快捷鍵區分大小寫。操作會套用至已標記的項目；若未標記任何項目，則使用游標所在項目。

| 按鍵 | 操作 |
| --- | --- |
| Tab | 切換窗格 |
| Up / Down、`k` / `j` | 移動游標 |
| PgUp / PgDn、Home / End、`g` / `G` | 換頁，或跳到第一個／最後一個項目 |
| Enter / Right / `l` | 進入目錄或預覽檔案 |
| Backspace / Left / `h` | 前往上層目錄 |
| Space / Insert | 切換選取狀態並向下移動 |
| Ctrl+A / `*` | 全選／反向選取 |
| `/` | 模糊篩選；Enter 保留，Esc 清除 |
| `.` | 切換隱藏檔案顯示 |
| `s` | 排序選單：`n` 名稱、`e` 副檔名、`s` 大小、`m` 修改時間、`r` 反向 |
| Ctrl+R | 重新整理目前窗格 |
| `x` | 項目操作選單 |
| `c` / `m` | 複製／移動至另一窗格的目錄 |
| `n` / `r` | 建立目錄／重新命名 |
| `d` / Delete | 確認後刪除 |
| `e` / `p` | 編輯／變更權限 |
| `v` / `i` | 全螢幕預覽／切換快速預覽 |
| `R` | 在本機顯示；遠端檔案先下載再顯示 |
| `o` / `S` | 主機選擇器／使用 sudo 重新連線遠端窗格 |
| `b` / `B` | 開啟書籤／將目前目錄加入書籤 |
| `=` / `t` | 比較目錄／在此開啟 shell |
| `Q` / `?` | 佇列面板／說明 |
| `q` / Ctrl+C | 結束；有傳輸進行時會先詢問 |

除了 `/` 以外，`..` 列會固定在最上方。按 Enter 或連按兩下可前往上層；此列不能標記或傳輸。目錄排在檔案之前。點擊欄位標題可依該欄位排序，再次點擊目前排序欄位可反轉順序。排序後游標仍停在相同項目，排序設定會依端點記住。

對話框可使用 Tab / Shift+Tab、Enter、Esc，以及顯示的字母快捷鍵。佇列中，Space 或 `p` 暫停／繼續，`c` 取消，`r` 重試，`d` 捨棄部分檔案，`R` 重試所有失敗項目，`C` 清除已完成工作。Tar 工作無法暫停；rsync 工作執行期間只能取消。

本機刪除會使用系統垃圾桶；若失敗，scpto 會提供永久刪除選項。遠端刪除為永久刪除，確認訊息會明確說明。

## 滑鼠與 Finder

點擊可聚焦窗格並移動游標。Ctrl+click 切換個別項目的選取狀態；Shift+click 選取範圍。連按兩下可進入目錄或預覽檔案。Option/Alt+click 開啟項目操作選單，也可使用 `x`。

各窗格標題中的端點按鈕可開啟主機選擇器。底部按鈕列會對目前窗格執行顯示的操作，並依可用寬度與操作模式調整。佇列各列有自己的控制按鈕。

在窗格間拖曳項目，可複製到目的目錄；放到資料夾列上則複製到該資料夾。拖到來源窗格內的資料夾會移動項目。Esc 取消拖曳。快速預覽窗格不接受放置。

Finder 拖放會以 bracketed paste 傳入包含 shell 引號或跳脫字元的絕對路徑。scpto 會確認每個路徑都存在，並在複製前詢問 **Upload / Cancel**。請先聚焦預期的遠端窗格：目的地依序為目前遠端窗格、另一個遠端窗格，若兩者皆為本機則使用目前本機窗格。外部拖放的滑鼠位置不決定目的地。終端機必須傳送 bracketed paste 才能使用此功能；詳見[手動拖放檢查表](docs/finder-drop-checklist.md)。

不支援從終端機拖出檔案到 Finder。對遠端檔案按 `R`，會下載到 `~/Downloads` 並在 Finder 顯示。名稱已存在時會改成 `name (1).ext`，再依序尋找可用編號。在 Linux，`R` 使用 `xdg-open` 開啟檔案所在目錄；本機檔案會直接顯示，不會複製。

## SSH 與 sudo

scpto 從 `~/.ssh/config` 探索明確的主機別名，支援 `Include` glob、循環偵測，以及最多 16 層的巢狀引用。萬用字元與否定的 `Host` 項目不會列出。主機詳細資訊透過 `ssh -G` 延後解析；選擇器可沿用 sshto 群組。

連線使用系統 `ssh`，每個主機別名各有一個 ControlMaster，位於 `/tmp/scpto-<uid>/<pid>` 下。目錄權限為私有的 0700。瀏覽與傳輸通道共用此 master。背景連線會停用設定中的遠端指令、TTY 請求、轉送與本機指令，避免干擾檔案傳輸。

SSH 密碼、金鑰密語及主機金鑰驗證完全由 OpenSSH 處理。scpto 不儲存密碼。互動式驗證時會暫停 TUI，將終端機交還 OpenSSH。正常結束會關閉 scpto 的 master；異常終止後，master 可能持續存在至 `ControlPersist=10m` 的閒置期限。

在遠端窗格按 `S`，可使用 sudo 重新連線。遠端必須有可執行的 `sftp-server`，且 sudoers 必須允許以 root 執行 `sh`。僅允許 `sftp-server` 的規則不足以使用此模式。密碼欄位會遮蔽輸入且不透露長度；機密緩衝區會清零，不會儲存或寫入紀錄，重新連線後若有需要會再次詢問。Sudo 端點使用逐檔 SFTP，不使用 tar 或 rsync 加速。

不支援 `requiretty`，顯示訊息如下：

```text
sudo on this host requires a terminal (requiretty), which an SFTP session cannot provide
```

## 傳輸

逐檔複製會在目的檔案旁寫入 `<name>.scpto-part`，完成後再重新命名。`partials.json` 記錄來源大小與修改時間；兩者都仍相符才可續傳。暫停與取消會保留部分檔案。重新執行傳輸可續傳，或在佇列選擇 **Discard partial**，刪除部分檔案及紀錄。SFTP server 若不支援 `posix-rename`，取代既有目的檔案時必須先刪除再重新命名，因此不是原子操作。

衝突對話框提供 **Overwrite**、**Skip**、**Rename**、**Overwrite if newer**，並在有相符部分檔案時提供 **Resume**，以及 **Apply to all in this job** 選項。Rename 會選擇未使用的 `name (N).ext`。名稱比對會考慮 Unicode NFC 等價名稱，以及目的檔案系統實際的查找行為。

預設門檻為展開目錄後的 200 個一般檔案項目。符合條件的本機至遠端或遠端至本機工作，在非 sudo 遠端具備 tar 時使用 tar。佇列會標示 **tar (no resume)**。Tar 指令路徑必須為 UTF-8；下載時各來源還必須有共同的上層目錄。此模式沒有 `.scpto-part` 保護，也不支援暫停或續傳。若最上層目的項目已存在，會顯示 **Overwrite all / Cancel** 對話框；中斷可能留下部分解壓縮的目的檔案。

遠端至遠端複製會經由本機記憶體串流傳送。符號連結會以連結形式複製；socket、裝置等特殊檔案會略過。跨端點移動只會在個別檔案複製成功後刪除來源，並在所有項目都成功後才移除來源目錄。

## 遠端編輯

按 `e` 依序使用 `$VISUAL`、`$EDITOR`，最後使用 `vi`。編輯器指令接受含引號的參數。編輯器執行時 TUI 會暫停。GUI 編輯器請使用等待參數：

```sh
VISUAL='code --wait' scpto my-server
```

遠端編輯使用私有、權限為 0600 的暫存檔。編輯器成功結束後，上傳已變更的內容。scpto 會重新檢查遠端大小與修改時間，若遠端檔案在此期間改變，覆寫前會先詢問。上傳失敗時會顯示保留的暫存路徑，供還原或重試；請在結束前取回內容，因為結束時會清除編輯暫存檔。

## 預覽

`v` 開啟全螢幕預覽；`i` 在另一窗格顯示即時快速預覽。文字與程式碼具有語法醒目提示，Markdown 會轉譯顯示（`m` 切換原始碼／轉譯），並支援 PNG、JPEG、GIF（第一幀）與 WebP 圖片。使用方向鍵、換頁鍵或滾輪捲動；Esc 或 `q` 關閉全螢幕預覽。

文字最多讀取 256 KiB，截斷時會顯示標記。二進位檔案顯示中繼資料。圖片預覽上限為 20 MiB。圖片呈現方式依終端機而異；在 tmux 內，scpto 使用半格字元顯示。遠端預覽使用瀏覽通道。

## 比較與同步

按 `=` 依正規化名稱、大小與修改時間比較兩個目錄樹，不計算內容雜湊。Space 選取項目；選取目錄也會選取其下項目。`>` 由左向右同步，`<` 由右向左同步，執行前會先確認。刪除已選取且僅存在於目的端的項目，必須在確認對話框中明確勾選；預設不勾選。

只有一端為本機、另一端為非 sudo 遠端，且雙方皆具備 Samba rsync >= 3.1 時，同步才使用 rsync。macOS openrsync 與 rsync 2.x 不符合條件。其他端點組合使用 SFTP／檔案傳輸引擎。這是針對選取項目的單向同步，不是背景鏡像。

## 重新連線與 VPN

連線中斷時，受影響的傳輸會等待，scpto 則以指數退避重新連線；預設從 1 秒開始，最長 30 秒。窗格會顯示重連倒數。驗證與主機金鑰問題需要 OpenSSH 互動處理，不會自動重試。

無法連線的主機可提示執行已設定的 VPN 指令：

```text
vpn|work-*|ssh -t vpn-gateway connect-vpn
```

規則格式為 `vpn|<glob>|<cmd>`，依不區分大小寫的別名或已解析主機名稱比對，第一個符合者生效。對話框會顯示指令，只有選擇 **Run** 後才透過 `/bin/sh -c` 執行，並暫停 TUI。指令成功後會立即重新連線。指令可在前兩個分隔符號之後包含管線符號。

## 設定

設定依序從非空的 `$SCPTO_CONFIG`、`$XDG_CONFIG_HOME/scpto/config`、`~/.config/scpto/config` 讀取。相對的 XDG 路徑會被忽略。預設位置沒有檔案時使用預設值；設定無效或無法讀取（包含明確指定的檔案不存在）時，會在啟動狀態列顯示警告並使用預設值。

使用 `set|key|value`，欄位前後不要加空白或引號。空白行與以 `#` 開頭的整行註解會被忽略；後面的設定覆蓋前面的值。所有接受的鍵值與預設值如下：

| 鍵值 | 預設值 | 說明 |
| --- | --- | --- |
| `concurrency` | `3` | 同時處理的逐檔項目數；整數 >= 1 |
| `tar-threshold` | `200` | 符合 tar 條件所需的一般檔案數；整數 >= 1 |
| `show-hidden` | `false` | 初始是否顯示點開頭檔案；`true` 或 `false` |
| `mtime-tolerance` | `0` | 視為相同的整秒誤差；整數 >= 0；FAT/exFAT 可試用 `2` |
| `reconnect-base-ms` | `1000` | 初始重連延遲，單位毫秒；整數 >= 1 |
| `reconnect-cap-ms` | `30000` | 最大延遲；整數 >= base |
| `keepalive-interval` | `15` | SSH keepalive 秒數；整數 >= 1 |

最後三個鍵值主要用於測試時控制時間。SSH keepalive 失敗次數固定為 3。

```text
set|concurrency|3
set|tar-threshold|200
set|show-hidden|false
set|mtime-tolerance|0
```

### sshto 群組

群組規則依序從非空的 `$SSHTO_CONFIG`、`$XDG_CONFIG_HOME/sshto/config`、`~/.config/sshto/config` 讀取。內容只當成資料解析，不會 source 或執行：

```text
group|Work
group|Personal
match|Work|alias|work-*
match|Personal|hostname|*.example.net
exclude|github.*
```

請先宣告群組再撰寫規則。比對欄位為 `alias`、`hostname` 或 `either`；依順序套用不區分大小寫的 glob 規則，第一個符合者生效。`All` 與 `Other` 為保留名稱；未符合的主機會出現在 Other。`exclude` 隱藏選擇器中符合的別名。以主機名稱為基礎的分組，可能隨 `ssh -G` 結果回傳而更新。預設位置沒有群組檔案時，不使用自訂群組；檔案無效或無法讀取時會顯示警告，並使用相同預設行為。

### 狀態

狀態位於 `$XDG_STATE_HOME/scpto`；若變數未設定或不是絕對路徑，則使用 `~/.local/state/scpto`。`state.json` 儲存書籤、上次目錄與排序；`partials.json` 儲存續傳中繼資料。多個執行個體同時使用時，以最後寫入者為準。損毀的 `state.json` 會備份為 `state.json.bak` 並顯示訊息，再以空白狀態啟動。

## 終端機注意事項

Ghostty、iTerm2、Terminal.app 與 tmux 必須將滑鼠事件及 bracketed paste 傳遞給 scpto。檔案拖放的跳脫格式與修飾鍵處理方式會隨終端機及設定而異；請使用[手動檢查表](docs/finder-drop-checklist.md) 驗證環境。

- 部分 Mac 終端機需要啟用 **Option as Meta/Alt** 才能使用 Option+click。`x` 與按鈕列提供鍵盤及滑鼠替代操作。
- Terminal.app 不會回報 Cmd；iTerm2 將 Cmd+click 保留給網址。選取請使用 Ctrl+click 或 Space。沒有 Insert 的 Mac 鍵盤可用 Space。
- 啟用滑鼠擷取後，終端機原生文字選取通常需要在 Terminal.app 按住 Option，或在其他終端機按住 Shift。
- 在 herdr/tmux 內，右鍵由 multiplexer 處理，scpto 會忽略它。
- macOS TCC 可能阻擋桌面、文件、下載及其他受保護資料夾。請在「系統設定」授予終端機 App「檔案與檔案夾」或「完整磁碟存取權」，然後重新啟動終端機 App 與 tmux/herdr 等 multiplexer，權限才會生效。
- 寬度不足 60 欄時，一次顯示一個窗格；Tab 或窗格分頁可切換。複製／移動仍以另一窗格為目的地。拖曳時可停留在另一分頁上切換窗格，或直接放到分頁以使用該目錄。寬度不足 40 欄或高度不足 10 列時，請放大終端機再繼續。

## 開發

設定與檢查請見 [CONTRIBUTING.md](CONTRIBUTING.md)，模組契約請見 [AGENTS.md](AGENTS.md)，版本歷史請見 [CHANGELOG.md](CHANGELOG.md)。

## 授權

[MIT](LICENSE)。Copyright (c) 2026 thisishugow。
