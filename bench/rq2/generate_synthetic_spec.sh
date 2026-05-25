#!/usr/bin/env bash

set -euo pipefail

if [[ $# -lt 2 ]]; then
    echo "usage: $0 <output-spec> <constraint-count> [--history-depth <n>] [--periodic-constraints <n>] [--periodic-hz <n>]" >&2
    exit 1
fi

output_spec="$1"
constraint_count="$2"
shift 2

history_depth="0"
periodic_constraints="0"
periodic_hz="100"

while [[ $# -gt 0 ]]; do
    case "$1" in
        --history-depth)
            history_depth="$2"
            shift 2
            ;;
        --periodic-constraints)
            periodic_constraints="$2"
            shift 2
            ;;
        --periodic-hz)
            periodic_hz="$2"
            shift 2
            ;;
        *)
            echo "unknown argument: $1" >&2
            exit 1
            ;;
    esac
done

if ! [[ "${constraint_count}" =~ ^[0-9]+$ ]] || [[ "${constraint_count}" -lt 1 ]]; then
    echo "constraint-count must be a positive integer" >&2
    exit 1
fi

if ! [[ "${history_depth}" =~ ^[0-9]+$ ]]; then
    echo "history-depth must be a non-negative integer" >&2
    exit 1
fi

if ! [[ "${periodic_constraints}" =~ ^[0-9]+$ ]]; then
    echo "periodic-constraints must be a non-negative integer" >&2
    exit 1
fi

if ! [[ "${periodic_hz}" =~ ^[0-9]+$ ]] || [[ "${periodic_hz}" -lt 1 ]]; then
    echo "periodic-hz must be a positive integer" >&2
    exit 1
fi

mkdir -p "$(dirname "${output_spec}")"

{
    cat <<'EOF'
module Benchmark {
    class Car {
        signals:
            Float lat;
            Float lon;
            Float speed;
EOF

    if [[ "${history_depth}" -gt 0 ]]; then
        cat <<'EOF'

        constraints:
EOF
        for ((i = 1; i <= constraint_count; i++)); do
            printf '            temporal_consistency_%03d {\n' "${i}"
            cat <<EOF
                if abs(
                    self.speed.last(default:0.0) -
                    self.speed.history(at:-${history_depth}, default:0.0)
                ) > 1000000.0 {
                    alert! "Temporal consistency violation.";
                }
            }

EOF
        done
    fi

    cat <<'EOF'
    }
}

world Rq2Synthetic {
    use Benchmark;
    cars: Car[];

    constraints:
EOF

    for ((i = 1; i <= constraint_count; i++)); do
        printf '        pairwise_distance_%03d @always {\n' "${i}"
        cat <<'EOF'
            if exists c1 in cars, c2 in cars:
                c1 != c2 and
                is_close(
                    c1.lat.last(default:0.0),
                    c1.lon.last(default:0.0),
                    c2.lat.last(default:0.0),
                    c2.lon.last(default:0.0),
                    0.25
                ) {
                alert! "Pairwise safe-distance violation.";
            }
        }

EOF
    done

    for ((i = 1; i <= periodic_constraints; i++)); do
        printf '        fleet_snapshot_%03d @%sHz {\n' "${i}" "${periodic_hz}"
        cat <<'EOF'
            if exists c in cars:
                c.speed.last(default:0.0) > 1000000.0 {
                alert! "Periodic fleet snapshot violation.";
            }
        }

EOF
    done

    cat <<'EOF'
    function is_close(x1:Float, y1:Float, x2:Float, y2:Float, threshold:Float) -> Bool {
        let distance = sqrt((x1 - x2)**2.0 + (y1 - y2)**2.0);
        distance < threshold
    }
}
EOF
} > "${output_spec}"
