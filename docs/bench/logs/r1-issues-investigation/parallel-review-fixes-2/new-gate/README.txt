Third run of the second review round's measurements on 4c8ce3f, under the new GPU gate
(gate-change.txt). Everything in the parent directory used the old 3 % gate.

Binaries (same files as ../builds.txt): final.exe ab91eaeb... diag build of 4c8ce3f,
mark.exe 1b21ccf6... diag build of feb14ed (679826a only adds docs), A 888ed490... integ-base
6b1e408 manifest, B 2e614d27... accept-r1 b5c06a7 manifest (warm-up program only).
Test and validation builds come from the investigate-r1 tree at dc725af (4c8ce3f plus
tools/bench changes only).

mib-forward.txt, mib-reverse.txt     1 MiB mixed soak, 60 s per arm, A/final/mark and mark/final/A
default-hotspots.txt                 hot_paths, 10 pairs
scale-{small,medium,large}.txt       scale::gradient, 12 / 8 / 6 pairs
scale-{small,large}-reverse.txt      confirmation pairs with the starting arm swapped, 12 / 6
paired-stats.txt                     all paired statistics
workspace.txt, gpu1-3.txt            cargo test --workspace --locked; cargo test --locked -p victory-gpu x3
ci-window.txt                        gpu-quiet window and per-step gates for those four runs
validation-window.txt, validation.json  tools/bench/run.ps1 -CheckOnly (Debug Layer + GPU-based validation)
gate-admissions.txt                  every gate admission: samples, waits, admitted desktop background
