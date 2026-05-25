# Shared Fragment Comparison Summary

Source manifest: `comparison_manifest.tsv`

| System | Status | Source file | Lines | Signals | Functions | Constraints | Aux flattening decls | Identity explicit | Pair flattening | History | Activation | Executable | Alignment |
| --- | --- | --- | ---: | ---: | ---: | ---: | ---: | --- | --- | --- | --- | --- | --- |
| OORV | executable | pairwise_distance.oorv | 33 | 3 | 1 | 1 | 0 | yes | no | last | @always | yes | exact |
| RTLola | executable | baselines/rtlola/pairwise_distance_two_cars.lola | 13 | 6 | 0 | 1 | 1 | yes | yes | hold().defaults | explicit_activation | yes | approximate |