use std::fs;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

fn temp_case(name: &str, spec: &str, csv: &str) -> (std::path::PathBuf, std::path::PathBuf) {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock should be after unix epoch")
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("oorv_{name}_{stamp}"));
    fs::create_dir_all(&dir).expect("temp dir should be created");
    let spec_path = dir.join("case.oorv");
    let csv_path = dir.join("case.csv");
    fs::write(&spec_path, spec).expect("spec should be written");
    fs::write(&csv_path, csv).expect("csv should be written");
    (spec_path, csv_path)
}

fn run_oorv(spec_path: &std::path::Path, csv_path: &std::path::Path) -> String {
    let exe = env!("CARGO_BIN_EXE_oorv");
    let output = Command::new(exe)
        .arg(spec_path)
        .arg("--offline")
        .arg("relative")
        .arg("--csv-in")
        .arg(csv_path)
        .arg("--verbosity")
        .arg("warnings")
        .output()
        .expect("oorv should launch");

    assert!(
        output.status.success(),
        "oorv failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    String::from_utf8(output.stdout).expect("stdout should be utf-8")
}

#[test]
fn always_alarm_fires_on_each_event() {
    let spec = r#"
world Fleet {
    constraints:
        heartbeat @always {
            if true {
                alert! "tick";
            }
        }
}
"#;
    let csv = "time\n0.0\n";
    let (spec_path, csv_path) = temp_case("always_alarm", spec, csv);

    let stdout = run_oorv(&spec_path, &csv_path);
    assert!(
        stdout.contains("tick"),
        "@always alarms should be evaluated on input events; stdout was:\n{stdout}"
    );
}

#[test]
fn forall_empty_domain_uses_vacuous_truth_for_expression_semantics() {
    let spec = r#"
module Vehicle {
    class Car {
        signals:
            Int speed;
    }
}

world Fleet {
    cars: Vehicle::Car[];

    constraints:
        no_active_cars @always {
            if forall car in cars: car.speed.last(default:-1) >= 0 {
                alert! "all active cars are nonnegative";
            }
        }
}
"#;
    let csv = "Vehicle::Car::uid,Vehicle::Car::speed,time\n#,#,0.0\n";
    let (spec_path, csv_path) = temp_case("forall_empty", spec, csv);

    let stdout = run_oorv(&spec_path, &csv_path);
    assert!(
        stdout.contains("all active cars are nonnegative"),
        "forall over an empty object domain should be true; stdout was:\n{stdout}"
    );
}

#[test]
fn exists_empty_domain_remains_false() {
    let spec = r#"
module Vehicle {
    class Car {
        signals:
            Int speed;
    }
}

world Fleet {
    cars: Vehicle::Car[];

    constraints:
        has_active_car @always {
            if exists car in cars: car.speed.last(default:1) > 0 {
                alert! "some active car exists";
            }
        }
}
"#;
    let csv = "Vehicle::Car::uid,Vehicle::Car::speed,time\n#,#,0.0\n";
    let (spec_path, csv_path) = temp_case("exists_empty", spec, csv);

    let stdout = run_oorv(&spec_path, &csv_path);
    assert!(
        !stdout.contains("some active car exists"),
        "exists over an empty object domain should be false; stdout was:\n{stdout}"
    );
}

#[test]
fn exists_pure_domain_true_when_active_object_exists() {
    let spec = r#"
module Vehicle {
    class Car {
        signals:
            Int speed;
    }
}

world Fleet {
    cars: Vehicle::Car[];

    constraints:
        has_active_car @always {
            if exists car in cars: true {
                alert! "car exists";
            }
        }
}
"#;
    let csv = "Vehicle::Car::uid,Vehicle::Car::speed,time\n1,7,0.0\n";
    let (spec_path, csv_path) = temp_case("exists_pure_domain", spec, csv);

    let stdout = run_oorv(&spec_path, &csv_path);
    assert!(
        stdout.contains("car exists"),
        "exists over a non-empty object domain should be true even without field access; stdout was:\n{stdout}"
    );
}

#[test]
fn forall_pure_domain_false_when_active_object_exists() {
    let spec = r#"
module Vehicle {
    class Car {
        signals:
            Int speed;
    }
}

world Fleet {
    cars: Vehicle::Car[];

    constraints:
        all_impossible @always {
            if forall car in cars: false {
                alert! "impossible";
            }
        }
}
"#;
    let csv = "Vehicle::Car::uid,Vehicle::Car::speed,time\n1,7,0.0\n";
    let (spec_path, csv_path) = temp_case("forall_pure_domain", spec, csv);

    let stdout = run_oorv(&spec_path, &csv_path);
    assert!(
        !stdout.contains("impossible"),
        "forall over a non-empty object domain should evaluate the body even without field access; stdout was:\n{stdout}"
    );
}

#[test]
fn quantified_repeated_access_uses_same_object_binding() {
    let spec = r#"
module Vehicle {
    class Car {
        signals:
            Int speed;
    }
}

world Fleet {
    cars: Vehicle::Car[];

    constraints:
        repeated_access @always {
            if exists car in cars:
                car.speed.last(default:0) >= car.speed.prev(default:0) {
                alert! "repeated access ok";
            }
        }
}
"#;
    let csv = "Vehicle::Car::uid,Vehicle::Car::speed,time\n1,5,0.0\n";
    let (spec_path, csv_path) = temp_case("repeated_access", spec, csv);

    let stdout = run_oorv(&spec_path, &csv_path);
    assert!(
        stdout.contains("repeated access ok"),
        "multiple accesses to the same quantified object should use one binding; stdout was:\n{stdout}"
    );
}

#[test]
fn quantified_same_domain_pair_allows_self_pair_without_explicit_distinctness() {
    let spec = r#"
module Vehicle {
    class Car {
        signals:
            Int speed;
    }
}

world Fleet {
    cars: Vehicle::Car[];

    constraints:
        self_pair_allowed @always {
            if exists a in cars, b in cars:
                a.speed.last(default:0) == b.speed.last(default:0) {
                alert! "same object pair allowed";
            }
        }
}
"#;
    let csv = "Vehicle::Car::uid,Vehicle::Car::speed,time\n1,7,0.0\n";
    let (spec_path, csv_path) = temp_case("self_pair_allowed", spec, csv);

    let stdout = run_oorv(&spec_path, &csv_path);
    assert!(
        stdout.contains("same object pair allowed"),
        "different quantified variables over the same domain may bind the same object unless the formula excludes it; stdout was:\n{stdout}"
    );
}

#[test]
fn quantified_explicit_distinctness_excludes_self_pair() {
    let spec = r#"
module Vehicle {
    class Car {
        signals:
            Int speed;
    }
}

world Fleet {
    cars: Vehicle::Car[];

    constraints:
        explicit_distinctness @always {
            if exists a in cars, b in cars:
                a != b && a.speed.last(default:0) == b.speed.last(default:0) {
                alert! "distinct pair";
            }
        }
}
"#;
    let csv = "Vehicle::Car::uid,Vehicle::Car::speed,time\n1,7,0.0\n";
    let (spec_path, csv_path) = temp_case("explicit_distinctness", spec, csv);

    let stdout = run_oorv(&spec_path, &csv_path);
    assert!(
        !stdout.contains("distinct pair"),
        "explicit inequality should exclude the only available self-pair; stdout was:\n{stdout}"
    );
}
