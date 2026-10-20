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

On Linux with Rust 1.99, `cargo test` passes all 16 shell tests, `cargo build --release`
succeeds, and `node --check` succeeds for both changed JavaScript files.
`cargo clippy --bin digup-shell` succeeds with three existing upstream warnings:
`manual_checked_ops` and `manual_is_multiple_of` in `status.rs`, and
`double_ended_iterator_last` in the speed-check parser in `main.rs`. Consequently
the additional experiment with `-D warnings` fails on those unchanged lines; no
warning suppression was added. Swift engine code was not changed or rebuilt.

### Isolated graphical result (2026-10-20)

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

Measured with the optimized Linux shell, system WebKitGTK 2.54.1 and the bundled
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
