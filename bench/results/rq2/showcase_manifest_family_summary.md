# RQ2 Showcase Family Summary

Source manifest: `results/rq2/showcase_manifest.tsv`

| Sweep | Axis span | Points | Median latency (ms) | Median throughput (events/s) | Repetitions |
| --- | --- | ---: | --- | --- | ---: |
| Object sweep | Objects 1-48 | 12 | 0.011-18.56 | 53.88-89,611 | 5 |
| Constraint sweep | Rules 1-24 | 12 | 0.818-19.08 | 52.42-1,222.2 | 5 |
| History sweep | Depth 0-20 | 10 | 4.828-4.909 | 203.7-207.1 | 5 |
| Periodic sweep | Periodic rules 0-20 | 10 | 4.813-4.917 | 203.4-207.8 | 5 |
| Mixed history+periodic sweep | Mixed level 0-20 | 9 | 6.442-6.616 | 151.1-155.2 | 3 |
| Bursty trace sweep | Burst size 1-16 | 6 | 1.575-6.134 | 163.0-635.0 | 3 |
| Rotating hot-set sweep | Hot-set size 4-24 | 6 | 1.446-6.550 | 152.7-691.6 | 3 |
| Long-run soak sweep | Events 320-25600 | 8 | 0.738-1.690 | 591.8-1,356.4 | 2 |
| Object x rule matrix | Objects 4-40 x Rules 2-20 | 49 | 0.054-40.54 | 24.66-18,396 | 3 |
