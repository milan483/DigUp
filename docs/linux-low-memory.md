# Linux idle memory

The Linux shell now defaults to releasing resources rather than keeping repeat
searches warm. In **Settings → General → Keep search ready in memory**, turn this
on to retain the previous behavior. The persisted setting is `keepWarm` (false by
default); other platforms keep their existing lifecycle.

* The search panel is created on demand. In low-memory mode, closing other windows
  first hides them. After all pages have been hidden for 30 seconds, they are
  **destroyed**, not just hidden. WebKit caches closed page processes, so their web
  processes are explicitly terminated before destruction. This is deferred while
  any page is visible to avoid terminating a process it might share. Queries and
  selections are discarded at destruction; main-window placement is saved on close.
* After 30 seconds without a completed interactive request, the search worker exits entirely,
  releasing its model, vector cache, database and runtime. Outstanding requests
  prevent retirement, even if their UI caller has gone away. A subsequent request
  starts a new worker, replaying the latest folder selection before searching.
* Index notifications do not start a sleeping search worker. A new worker opens
  the latest index. Indexing, filesystem watchers, tray actions, global shortcuts
  and single-instance activation stay in the shell and continue independently.
* Retirement is checked every five seconds. Worker termination occurs off the UI
  thread; panel visibility and destruction are serialized on the UI thread.
  Quit permanently closes each helper before stopping it, so late notifications
  cannot restart search, indexing or model download processes.

Panel activation (including the D-Bus single-instance callback and KDE hotkey
thread) queues the **entire** lookup/toggle/create/show transaction onto the UI
thread. Cleanup cannot destroy a hidden panel between lookup and show. Main-window,
settings and setup activation already queue their creation through `later`; setup's
initial creation runs on the main thread. Panel show failures are now logged.

The tradeoff is a cold panel load and, for meaning searches, a model/vector load
after idle. There is no memory limit that would truncate searches or indexing.
An open settings page or main window still needs a browser. Destroying all pages
does not guarantee WebKit will return every allocation or stop its shared network
process; the GTK/Tauri shell itself remains resident. This is not a separate tiny
native supervisor and should not be described as a zero-browser resident process.

## Checks

Run `cargo test` in `shell/src-tauri` (GTK/WebKit development libraries and Python 3
are required on Linux). The process-level search lifecycle tests use isolated fake
JSON-line workers and cover expiry cancellation, pending/abandoned requests,
recreation, requests racing retirement, latest folder replay, non-waking index
notifications, independent indexer survival, and final shutdown/restart rejection.
No models or personal index are needed.

For graphical measurements use a private D-Bus session, temporary HOME and XDG
directories, explicit `--settings` and `--index`, and an empty test folder. Never
run a test second instance against your normal session: single-instance activation
would reach your installed DigUp. Sum `Pss` from `/proc/<pid>/smaps_rollup` for the
shell **and descendants**, before and after hiding all windows for at least 35
seconds. Compare `keepWarm: true` and false with the same binary and workload;
report the shell, WebKit and worker separately. Repeat activation after idle and
verify a new panel/worker answers, then use Quit to verify helper cleanup. Model
inference and real indexing need a separate fixture if they are to be measured.

### Local build verification

On Linux with Rust 1.99, `cargo test` passes 17 shell tests and
`cargo test --features fork-no-updates` passes all 18 shell tests. `cargo build --release --features fork-no-updates`
succeeds, and `node --check` succeeds for both changed JavaScript files.
`cargo test --release --features fork-no-updates` also passes all 18 tests.
`cargo clippy --bin digup-shell` succeeds with three existing upstream warnings:
`manual_checked_ops` and `manual_is_multiple_of` in `status.rs`, and
`double_ended_iterator_last` in the speed-check parser in `main.rs`. Consequently
the additional experiment with `-D warnings` fails on those unchanged lines; no
warning suppression was added. Swift engine code was not changed or rebuilt.

### Earlier isolated graphical result (2026-10-20, before activation-race correction)

Reproduce with a built shell and compatible CLI (GTK/WebKit libraries, an X11
display and `dbus-run-session` are required):

```sh
python3 scripts/measure-linux-idle.py \
  shell/src-tauri/target/release/digup-shell /absolute/path/to/digup
```

The script also requires `xdotool` for the main window's Ctrl+W check. It creates
a private session bus and temporary HOME/XDG/settings/index,
blocks model downloads, uses no source folders, and retains logs and JSON in the
printed fixture directory. It checks absence of the idle worker and web process,
their recreation on panel and main-window activation, and successful frontend-driven `quit()` (the
existing debug-report route, not simulated menu clicks). Its measurement instances
are separately terminated after sampling. No installed application is replaced.

The following historical measurements preceded the activation-race fix; they are
indicative, not verification of the corrected package. Measured with the optimized Linux shell, system WebKitGTK 2.54.1 and the bundled
0.7.0-beta.2 CLI, after opening the panel, hiding it and waiting 40 seconds:

| Idle PSS, MiB | Keep warm | Default low memory |
| --- | ---: | ---: |
| Shell | 79.50 | 90.43 |
| WebKit network process | 32.45 | 35.87 |
| WebKit web process | 118.91 | absent |
| Search worker | 13.85 | absent |
| **Total** | **244.71** | **126.30** |

This is **118.41 MiB (48.4%) less idle PSS** in this model-free, empty-index
fixture. Preceding runs measured 232.47 → 126.40 and 245.58 → 127.07 MiB. PSS varies with other
processes sharing libraries; the shell/network PSS can rise when their web process
exits. These are not an apples-to-apples comparison with an installed app's populated
index or loaded model, nor a guarantee of the same savings on other systems.

Reactivation created a new WebKit web process and search worker; the default-mode
total was 247.40 MiB at the six-second observation point. Opening the main window,
closing it with Ctrl+W and waiting 40 seconds left only the shell and network
process at **131.14 MiB**; desktop activation then recreated the main window and
search worker (258.18 MiB at the five-second observation point). These are functional
reopen checks, not startup-latency benchmarks. Frontend-driven Quit exited with
status 0 in both modes. Independent indexing is verified by the process-lifecycle
unit fixture, not a real indexing/model-inference workload in this measurement.
The network process and approximately 90 MiB shell remain resident: this delivers
substantial reclamation, not the footprint of a separate tiny native supervisor.

## Low-memory fork AppImage

This is an experimental **milan483/DigUp** fork, not an official DigUp release.
Build with `cargo build --release --features fork-no-updates` in `shell/src-tauri`.
That compile-time feature disables upstream automatic/manual update checks, offers,
downloads and installation, including debug update flags, without changing anyone's
saved update preferences. Manual Check for Updates explains the fork policy.
Obtain new fork builds manually; do not expect the official updater to maintain it.

`scripts/package-low-memory-appimage.sh` takes a trusted extracted official AppDir,
the fork binary, output path, official appimagetool and patchelf paths. It copies
the full original runtime/helper/resources/licenses, replaces only the shell,
restores its `$ORIGIN/../lib` RUNPATH, labels the desktop entry as a fork and adds
this document. It refuses a binary without the fork update-policy message.
The Swift helper is reused, not rebuilt. The package retains the existing app ID
and data formats intentionally: a normal launch uses your existing settings/index.
**Quit the installed app first** before choosing to use this fork (otherwise
single-instance activation reaches the already running app). No installer, login
entry or managed application needs replacing: run the downloaded executable.

This locally compiled shell requires **GLIBC 2.39** (the original shell required
2.34). It is tested only on this machine, not a general Linux compatibility build.
An AppImage bundles many libraries but does not remove that requirement.

The enhanced isolated measurement types a real query after reopening and validates
the actual engine's successful search reply (`hits`, even when empty), rather than
merely checking for a worker process. It repeats searches around the 30-second
cleanup boundary at 29.8, 30.2 and 35.1 seconds. A structural unit regression guards
dispatch-before-lookup on both panel activation entry points. Frontend debug-report
Quit also runs a real query and tracks shell descendants until all are gone; it
does not substitute SIGTERM for that Quit assertion. Download blocking requires a
test helper proxy, which forwards all search requests to the packaged real helper.
The proxy adds a small Python-process overhead to new measurements.
It uses the system Python with `-E`: AppRun exports bundled Python paths that
otherwise prevent the test proxy from importing its standard library. This does
not change the real helper's packaged environment.

### Packaged executable smoke check (2026-10-20)

The locally generated AppImage was launched normally (through its AppImage
runtime, not just its extracted shell), on a private D-Bus session with temporary
HOME/XDG/settings/index/model paths and downloads blocked by the test proxy.
The main-window frontend's `package-smoke` query received a successful real
engine reply with `hits: []`, then frontend Quit exited with status 0.
Even with update checks enabled in the fixture settings and both debug update
flags supplied, no upstream update run was started. The original helper,
resources, library symlinks and third-party license notices are preserved.

This smoke check is model-free. It does not establish real-index inference,
nonempty search relevance, media preview support or broad Linux compatibility.

### Corrected packaged lifecycle result (2026-10-20)

The enhanced script passed against the extracted final package's **AppRun** and
its bundled real helper, exercising the package's runtime environment as well as
the activation serialization correction. Idle sampling followed 40 seconds hidden:

| Idle PSS, MiB | Keep warm | Default low memory |
| --- | ---: | ---: |
| Shell | 71.57 | 86.85 |
| WebKit network process | 34.99 | 42.12 |
| WebKit web process | 135.41 | absent |
| Real search worker | 22.06 | absent |
| Test-only Python proxy | 6.48 | absent |
| **Total** | **270.51** | **128.97** |

This is **141.54 MiB (52.3%) less idle PSS** for this empty-index, model-free
fixture, including the test proxy overhead in keep-warm mode. It is not a
measurement of savings with a populated personal index or loaded model.
The shell and shared network process remain resident.

Both modes answered real frontend searches after reopening. Low-memory mode also
answered all three searches after activation at 29.8, 30.2 and 35.1 seconds hidden.
Main-window Ctrl+W reclaimed the worker and web process (132.93 MiB total idle);
desktop reactivation recreated them and answered another real query.
Frontend-driven Quit returned 0 in both modes after a successful real engine
reply, with no surviving tracked descendants. The local evidence is retained in
`/tmp/opencode/digup-final-packaged-runtime-fixed.json` and the fixture directory
named there. A separate normal AppImage-runtime smoke check passed as described
above; only this documentation changed when regenerating the final artifact.
