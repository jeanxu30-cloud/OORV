use std::fs;
use std::path::Path;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

fn temp_spec(name: &str, spec: &str) -> std::path::PathBuf {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock should be after unix epoch")
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("oorv2c_{name}_{stamp}"));
    fs::create_dir_all(&dir).expect("temp dir should be created");
    let spec_path = dir.join("case.oorv");
    fs::write(&spec_path, spec).expect("spec should be written");
    spec_path
}

fn c_compiler() -> Option<&'static str> {
    ["cc", "clang", "gcc"]
        .into_iter()
        .find(|candidate| Command::new(candidate).arg("--version").output().is_ok())
}

fn compile_generated_c(source: &Path) {
    let Some(compiler) = c_compiler() else {
        eprintln!("skipping generated C compilation check: no C compiler found");
        return;
    };
    let object = source.with_extension("o");
    let output = Command::new(compiler)
        .arg("-std=c11")
        .arg("-Wall")
        .arg("-Wextra")
        .arg("-Werror")
        .arg("-c")
        .arg(source)
        .arg("-o")
        .arg(&object)
        .output()
        .expect("C compiler should launch");

    assert!(
        output.status.success(),
        "generated C failed to compile with {compiler}\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn compile_and_run_harness(source: &Path) {
    let Some(compiler) = c_compiler() else {
        eprintln!("skipping generated C harness check: no C compiler found");
        return;
    };
    let exe = source.with_extension(std::env::consts::EXE_EXTENSION);
    let output = Command::new(compiler)
        .arg("-std=c11")
        .arg("-Wall")
        .arg("-Wextra")
        .arg("-Werror")
        .arg(source)
        .arg("-o")
        .arg(&exe)
        .output()
        .expect("C compiler should launch");

    assert!(
        output.status.success(),
        "generated C harness failed to compile with {compiler}\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let output = Command::new(&exe)
        .output()
        .expect("generated C harness should launch");

    assert!(
        output.status.success(),
        "generated C harness failed at runtime\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn c_include_path(path: &Path) -> String {
    path.display()
        .to_string()
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
}

#[test]
fn oorv2c_uses_core_pipeline_and_emits_c_metadata() {
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
        moving @always {
            if exists car in cars: car.speed.last(default:0) > 0 {
                alert! "moving";
            }
        }
}
"#;
    let spec_path = temp_spec("metadata", spec);
    let exe = env!("CARGO_BIN_EXE_oorv2c");
    let output = Command::new(exe)
        .arg(&spec_path)
        .output()
        .expect("oorv2c should launch");

    assert!(
        output.status.success(),
        "oorv2c failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8(output.stdout).expect("stdout should be utf-8");
    assert!(stdout.contains("#include <stdbool.h>"));
    assert!(stdout.contains("OORV_INPUT_COUNT = 2"));
    assert!(stdout.contains("OORV_ALARM_COUNT = 1"));
    assert!(stdout.contains("Vehicle::Car::speed"));
    assert!(stdout.contains("size_t oorv_monitor_input_count(void)"));
    assert!(stdout.contains("const char *oorv_monitor_alarm_name(size_t index)"));

    let c_path = spec_path
        .parent()
        .expect("temp spec should have a parent directory")
        .join("generated.c");
    let output = Command::new(exe)
        .arg(&spec_path)
        .arg("--output")
        .arg(&c_path)
        .output()
        .expect("oorv2c should launch");

    assert!(
        output.status.success(),
        "oorv2c --output failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let generated = fs::read_to_string(&c_path).expect("generated C should be readable");
    assert!(generated.contains("#define OORV_GENERATED_SOURCE"));
    assert!(generated.contains("static const char *const OORV_INPUT_STREAMS[]"));
    assert!(generated.contains("void oorv_monitor_describe(FILE *out)"));
    compile_generated_c(&c_path);
}

#[test]
fn oorv2c_emits_executable_c_for_stateless_event_alarm_subset() {
    let spec = r#"
module Car {
    class Car {
        signals:
            Int a;
            Int b;

        constraints:
            positive_sum_check {
                if self.a + self.b > 0 {
                    alert! "positive sum";
                }
            }
    }
}

world TrafficSystem {
    cars: Car::Car[];
}
"#;
    let spec_path = temp_spec("executable_alarm", spec);
    let c_path = spec_path
        .parent()
        .expect("temp spec should have a parent directory")
        .join("generated.c");
    let exe = env!("CARGO_BIN_EXE_oorv2c");
    let output = Command::new(exe)
        .arg(&spec_path)
        .arg("--output")
        .arg(&c_path)
        .output()
        .expect("oorv2c should launch");

    assert!(
        output.status.success(),
        "oorv2c --output failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let generated = fs::read_to_string(&c_path).expect("generated C should be readable");
    assert!(generated.contains("OORV_EXECUTABLE_ALARM_COUNT = 1"));
    assert!(generated.contains("OORV_UNSUPPORTED_ALARM_COUNT = 0"));
    assert!(generated.contains("typedef struct OorvEvent"));
    assert!(generated.contains("size_t oorv_monitor_step_event("));
    assert!(generated.contains("bool oorv_monitor_eval_alarm_0("));
    compile_generated_c(&c_path);

    let harness_path = c_path
        .parent()
        .expect("generated C should have a parent directory")
        .join("harness.c");
    let harness = format!(
        r#"
#include <assert.h>
#include <stddef.h>
#include "{generated}"

int main(void) {{
    OorvMonitor monitor;
    OorvEvent event;
    size_t alarms[2] = {{99, 99}};

    oorv_monitor_init(&monitor);
    oorv_event_clear(&event);

    /* Signal order for this lowered IR is uid, a, b. */
    oorv_event_set_i64(&event, 0, 7);
    oorv_event_set_i64(&event, 1, 2);
    oorv_event_set_i64(&event, 2, -1);
    size_t fired = oorv_monitor_step_event(&monitor, &event, alarms, 2);
    assert(fired == 1);
    assert(alarms[0] == 0);

    oorv_event_clear(&event);
    oorv_event_set_i64(&event, 0, 7);
    oorv_event_set_i64(&event, 1, -5);
    oorv_event_set_i64(&event, 2, 2);
    fired = oorv_monitor_step_event(&monitor, &event, alarms, 2);
    assert(fired == 0);
    assert(monitor.event_count == 2);

    return 0;
}}
"#,
        generated = c_include_path(&c_path)
    );
    fs::write(&harness_path, harness).expect("harness should be written");
    compile_and_run_harness(&harness_path);
}
