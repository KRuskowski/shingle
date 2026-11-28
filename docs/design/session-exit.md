# Saving the session at exit

Built in workspace `shingle-copy-and-exit` (2026-11-28). Code: crates/shingle-session/src/writer.rs (`SessionWriter::finish`), crates/shingle-session/src/sync.rs (`SyncCommand`), crates/shingle-core/src/session.rs (`SessionSnapshot::save_atomic_with`), crates/shingle-frontend/src/frontend.rs (`Frontend::shutdown`), crates/shingle-bin/src/headless.rs (`shut_down`).

## The problem

Several harness runs on 2026-11-28 timed out Shingle's exit after SIGTERM at 75 to 80 s, with the process in fsync (`folio_wait_bit_common`, system call 74) while other builds kept the host's `/home` busy. The exit did three things that waited for the disk, one after the other:

1. `SessionWriter::finish` joined the writer thread, which first finished the background save in progress: an fsync of the temporary file, the rename and an fsync of the directory.
2. It then wrote the final snapshot on the main thread with the same two fsyncs.
3. A thread in fsync waits for the disk regardless of signals, and a process cannot finish exiting while any of its threads still does, so not even a detached thread would have let the exit through. On FUSE a request already sent to the daemon is waited out the same way.

Each fsync took 8 to 17 s on that disk before ([status.md](../status.md#sessions)), more while builds ran. The session harness's new `exit` group reproduces it: with every fsync held for 20 s by tools/session_test/slowfs.py and one in progress, the build before the change took 39.4 s to exit after SIGTERM.

## Decision

The exit waits for no disk, and the session stays as durable as before up to the last background save.

- Background saves still sync the temporary file before the rename and the directory after it, but each fsync runs in a child process, `sync FILE` (coreutils syncs each file named with fsync), which the writer thread waits for while checking every 5 ms whether the final snapshot has arrived. Without a `sync` program the writer syncs in place as before.
- `SessionWriter::finish` hands the final snapshot to the writer thread, after every earlier one, so an older snapshot can never be renamed over it. A save still waiting is dropped. A save in progress stops waiting for its child and leaves its temporary file in place, since removing it (or even closing it) would wait for that fsync on FUSE; the session file then still holds the last completed save. The final snapshot is written and renamed into place without a sync. `finish` waits at most 2 s (`FINISH_LIMIT`) and then lets the exit go on.
- The given-up child finishes its fsync on its own; nothing of Shingle waits for it. The next writer to start removes the temporary files of given-up saves (`SessionStore::remove_given_up_writes`).
- The headless backend, which saves only at exit, saves without a sync too.

Why no fsync at exit is needed: a crash of Shingle cannot lose the final snapshot once it is renamed into place, since it is in the page cache, and the kernel writes it back within its writeback delay (30 s by default). Only a crash of the whole system in that window can lose it, and then the file holds the last synced save on ext4, whose rename over an existing file allocates the new file's blocks first (`auto_da_alloc`); a file system without that can leave a truncated file, which Shingle sets aside as corrupt at the next start ([status.md](../status.md#sessions)). The layout changes between the last synced save and the exit are what can be lost, which is the same as a crash a moment earlier.

The alternatives considered:

- A bounded fsync at exit, on a thread that is abandoned after a timeout: the process still cannot exit until the fsync returns, so the bound does not hold.
- Syncing only the directory entry: the directory fsync was not the slow part, and without the file's fsync the rename can reach the disk before the data.
- Relying on earlier saves alone and writing nothing at exit: the background save in progress at SIGTERM is usually the newest state, and dropping it loses more than an unsynced final write does.

## What Wayfire does

Wayfire saves no session and writes nothing at exit: neither its source nor its plugins call fsync. SIGTERM and SIGINT make its handler call `shutdown` (wayfire:src/main.cpp:113-137), which ends the event loop with `wl_display_terminate` (wayfire:src/core/core.cpp:312-327); `main` then frees the core (wayfire:src/main.cpp:514-524), whose `fini` unloads the plugins, stops Xwayland, disconnects the clients and destroys the display (wayfire:src/core/core.cpp:342-372). Its config file is only read. Shingle's exit now matches it in waiting for no disk, and still writes the session.

## Checks

- Unit tests: crates/shingle-session/src/writer.rs (`a_hanging_sync_keeps_the_last_save_and_lets_the_exit_through`, a sync that hangs neither loses the last completed save nor holds up `finish`, and the next writer removes the given-up file; `finish_gives_up_the_sync_in_progress_and_writes_last_unsynced`; `finish_gives_up_on_a_final_write_that_hangs`), crates/shingle-session/src/sync.rs and crates/shingle-core/src/session.rs.
- `tools/session_test/run.py --checks exit`: nested Shingle on a slowfs mount whose fsyncs take 20 s, served by a thread per request, must exit with status 0 within 5 s of SIGTERM while a session fsync is in progress, and the session file must hold the layout set just before.
