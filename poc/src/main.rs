//! clockcal P1 PoC: find the Windows 11 taskbar clock via UI Automation and test
//! swallowing clicks on it with a WH_MOUSE_LL hook.
//!
//!   clockcal-poc list
//!   clockcal-poc hook [--swallow] [--timeout SECS] [--refresh-ms N] [--ignore-injected]
//!                     [--simulate-hook-loss SECS] [--miss-expire N]
//!   clockcal-poc nudge [--count N]     (tiny relative mouse moves, no clicks; for testing)

mod hook;
mod logx;
mod uia;

use std::process::ExitCode;
use std::thread;
use std::time::Duration;

use windows::Win32::Foundation::HWND;
use windows::Win32::System::Com::{CoInitializeEx, CoUninitialize, COINIT_MULTITHREADED};
use windows::Win32::UI::HiDpi::{
    AreDpiAwarenessContextsEqual, GetThreadDpiAwarenessContext, SetProcessDpiAwarenessContext,
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    SendInput, INPUT, INPUT_0, INPUT_MOUSE, MOUSEEVENTF_MOVE, MOUSEINPUT,
};

const TIMEOUT_DEFAULT: u64 = 30;
const TIMEOUT_MAX: u64 = 120;

fn set_dpi() -> String {
    unsafe {
        let set = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        let is_v2 = AreDpiAwarenessContextsEqual(
            GetThreadDpiAwarenessContext(),
            DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
        )
        .as_bool();
        format!("SetProcessDpiAwarenessContext(PMv2) ok={} thread_is_PMv2={}", set.is_ok(), is_v2)
    }
}

fn usage() -> ExitCode {
    eprintln!(
        "usage:\n  clockcal-poc list\n  clockcal-poc hook [--swallow] [--timeout SECS(default 30, max 120)] [--refresh-ms N(default 1500)] [--ignore-injected] [--simulate-hook-loss SECS] [--miss-expire N(default 20 timer misses)]\n  clockcal-poc nudge [--count N]"
    );
    ExitCode::from(64)
}

fn cmd_list() -> ExitCode {
    // UIA on its own MTA thread.
    let r = thread::spawn(|| unsafe {
        if let Err(e) = CoInitializeEx(None, COINIT_MULTITHREADED).ok() {
            println!("CoInitializeEx failed: {e}");
            return 1u8;
        }
        let code = match uia::Scanner::new() {
            Err(e) => {
                println!("UIA init failed: {e}");
                1
            }
            Ok(sc) => {
                let scans = sc.scan();
                if scans.is_empty() {
                    println!("no taskbar windows found");
                }
                for s in &scans {
                    println!("== taskbar {} hwnd={:?}", s.class, s.hwnd.0);
                    if let Some(e) = &s.error {
                        println!("   UIA error: {e}");
                    }
                    for (i, b) in s.buttons.iter().enumerate() {
                        let mark = match s.clock {
                            Some((ci, rule)) if ci == i => format!("  <== CLOCK ({rule})"),
                            _ => String::new(),
                        };
                        println!(
                            "   [{i}] class={} aid={:?} name={:?}\n       rect {} valid={} offscreen={}\n       monitor {} {}{mark}",
                            b.class_name,
                            b.automation_id,
                            b.name,
                            uia::fmt_rect(&b.rect),
                            b.valid,
                            b.offscreen,
                            b.monitor,
                            uia::fmt_rect(&b.monitor_rect),
                        );
                    }
                    if s.clock.is_none() {
                        println!("   no clock matched");
                    }
                    match sc.related(HWND(s.hwnd.0)) {
                        Ok(rel) => {
                            println!("   related *OmniButton* classes (context only, not matched):");
                            for b in rel {
                                println!(
                                    "     class={} aid={:?} rect {}",
                                    b.class_name,
                                    b.automation_id,
                                    uia::fmt_rect(&b.rect)
                                );
                            }
                        }
                        Err(e) => println!("   related walk failed: {e}"),
                    }
                }
                0
            }
        };
        CoUninitialize();
        code
    })
    .join()
    .unwrap_or(1);
    ExitCode::from(r)
}

fn cmd_nudge(count: u32) -> ExitCode {
    // Relative +3/-3 px moves only. No button flags are ever set.
    let mk = |dx: i32| INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT { dx, dy: 0, mouseData: 0, dwFlags: MOUSEEVENTF_MOVE, time: 0, dwExtraInfo: 0 },
        },
    };
    let mut sent = 0;
    for _ in 0..count {
        for dx in [3, -3] {
            sent += unsafe { SendInput(&[mk(dx)], std::mem::size_of::<INPUT>() as i32) };
            thread::sleep(Duration::from_millis(30));
        }
    }
    println!("nudge: sent {sent} relative move inputs (no clicks)");
    ExitCode::SUCCESS
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some(cmd) = args.first().cloned() else { return usage() };
    let dpi = set_dpi();

    let num = |flag: &str| -> Option<Result<u64, String>> {
        args.iter().position(|a| a == flag).map(|i| {
            args.get(i + 1)
                .ok_or(format!("{flag} needs a value"))
                .and_then(|v| v.parse::<u64>().map_err(|e| format!("{flag}: {e}")))
        })
    };

    match cmd.as_str() {
        "list" => {
            println!("{dpi}");
            cmd_list()
        }
        "nudge" => match num("--count").unwrap_or(Ok(20)) {
            Ok(n) => cmd_nudge(n.min(200) as u32),
            Err(e) => {
                eprintln!("{e}");
                usage()
            }
        },
        "hook" => {
            let parse = || -> Result<hook::Opts, String> {
                let mut timeout_s = num("--timeout").unwrap_or(Ok(TIMEOUT_DEFAULT))?;
                if timeout_s == 0 {
                    return Err("--timeout must be >= 1".into());
                }
                if timeout_s > TIMEOUT_MAX {
                    eprintln!("--timeout {timeout_s} clamped to hard maximum {TIMEOUT_MAX}");
                    timeout_s = TIMEOUT_MAX;
                }
                let refresh_ms = num("--refresh-ms").unwrap_or(Ok(1500))?.clamp(100, 60_000) as u32;
                let simulate_loss_s = num("--simulate-hook-loss").transpose()?;
                let miss_expire = num("--miss-expire").unwrap_or(Ok(20))?.clamp(1, 10_000) as u32;
                Ok(hook::Opts {
                    miss_expire,
                    swallow: args.iter().any(|a| a == "--swallow"),
                    timeout_s,
                    refresh_ms,
                    ignore_injected: args.iter().any(|a| a == "--ignore-injected"),
                    simulate_loss_s,
                })
            };
            let o = match parse() {
                Ok(o) => o,
                Err(e) => {
                    eprintln!("{e}");
                    return usage();
                }
            };
            match logx::init("hook") {
                Ok(p) => logln!("[main] log file {}", p.display()),
                Err(e) => eprintln!("log file not created: {e}"),
            }
            logln!("[main] {dpi}");
            ExitCode::from(hook::run(o) as u8)
        }
        _ => usage(),
    }
}
