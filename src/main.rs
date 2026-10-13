mod cli;
mod config;
mod console_log;
mod interactive_menu;
mod keyboard_hook;
mod layout_indicator;
mod layout_manager;
mod layout_switcher;
mod lifecycle_log;

use cli::{create_mutex, execute_command, parse_args, should_run_in_background, CliCommand};
use interactive_menu::show_interactive_menu;
use keyboard_hook::{initialize_layout_switching, install_hook, uninstall_hook};
use std::ffi::OsString;
use std::mem;
use std::os::windows::ffi::OsStrExt;
use std::ptr;
use std::sync::atomic::{AtomicPtr, Ordering};
use winapi::shared::minwindef::*;
use winapi::shared::windef::*;
use winapi::um::errhandlingapi::GetLastError;
use winapi::um::handleapi::CloseHandle;
use winapi::um::processthreadsapi::GetCurrentThreadId;
use winapi::um::wincon::*;
use winapi::um::winuser::*;

// Global atomic pointer to store mutex handle
static MUTEX_HANDLE: AtomicPtr<winapi::ctypes::c_void> = AtomicPtr::new(ptr::null_mut());

fn main() {
    std::panic::set_hook(Box::new(|info| {
        lifecycle_log::event(&format!("panic: {info}"))
    }));
    // Keep fullscreen client and monitor bounds in physical pixels on scaled displays.
    unsafe {
        SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }
    // Parse command line arguments
    let command = parse_args();

    // Handle CLI commands that don't require running the main loop
    match command {
        CliCommand::Enable(_)
        | CliCommand::Disable
        | CliCommand::Start(_)
        | CliCommand::Stop
        | CliCommand::Status
        | CliCommand::Help
        | CliCommand::Version
        | CliCommand::Removed(_)
        | CliCommand::Unknown(_) => {
            let (exit_code, _) = execute_command(command);
            std::process::exit(exit_code);
        }
        CliCommand::Background(country_codes) => {
            // Execute background-specific logic and get country codes
            let (_, final_country_codes) =
                execute_command(CliCommand::Background(country_codes.clone()));

            // Load configuration from file if no country codes provided via command line
            let country_codes_to_use = if final_country_codes.is_empty() {
                let config = config::load_config();
                config.country_codes
            } else {
                final_country_codes
            };

            // Validate country codes if provided
            if !country_codes_to_use.is_empty() {
                if let Err(error) = layout_manager::validate_country_codes(
                    &country_codes_to_use
                        .iter()
                        .map(|s| s.as_str())
                        .collect::<Vec<_>>(),
                ) {
                    eprintln!("Error: {}", error);
                    std::process::exit(1);
                }
            }

            // Continue with normal execution after background setup
            run_main_loop(country_codes_to_use);
        }
        CliCommand::Menu => {
            // Show interactive menu when no parameters provided
            let (menu_result, country_codes) = show_interactive_menu();
            if menu_result != 0 {
                std::process::exit(menu_result);
            }
            // If menu_result is 0, continue with normal execution using country codes from menu
            run_main_loop(country_codes);
        }
        CliCommand::Run(country_codes) => {
            // Validate country codes if provided
            if !country_codes.is_empty() {
                if let Err(error) = layout_manager::validate_country_codes(
                    &country_codes.iter().map(|s| s.as_str()).collect::<Vec<_>>(),
                ) {
                    eprintln!("Error: {}", error);
                    std::process::exit(1);
                }
            }

            // Direct run mode - continue with normal execution
            run_main_loop(country_codes);
        }
    }
}

fn run_main_loop(country_codes: Vec<String>) {
    let config = config::load_config();
    layout_indicator::configure(config.scroll_lock_indicator);
    keyboard_hook::configure_fullscreen_passthrough(config.fullscreen_passthrough);
    // Create mutex to prevent multiple instances
    let mutex = create_mutex();
    if mutex.is_null() {
        lifecycle_log::event(&format!("startup failed: mutex error {}", unsafe {
            GetLastError()
        }));
        eprintln!("Failed to create mutex");
        return;
    }

    // Store mutex handle in atomic pointer
    MUTEX_HANDLE.store(mutex, Ordering::SeqCst);

    // Check if another instance is already running
    unsafe {
        if GetLastError() == winapi::shared::winerror::ERROR_ALREADY_EXISTS {
            if !should_run_in_background() {
                println!("The program is already running in the background.");
            }
            CloseHandle(mutex);
            return;
        }
    }

    let is_background = should_run_in_background();
    lifecycle_log::event(&format!(
        "starting version={} background={is_background}",
        env!("CARGO_PKG_VERSION")
    ));

    if is_background {
        // Detaching from the parent process in the background
        unsafe {
            detach_from_console();
        }
    } else {
        // Print a line for every switch and for indicator changes
        console_log::enable();
    }

    unsafe {
        // Initialize layout switching with country codes
        initialize_layout_switching(&country_codes);

        // Set initial Scroll Lock state based on current layout
        layout_indicator::update_layout_indicator();

        // Install the hook
        match install_hook() {
            Ok(()) => {
                // Ensure CapsLock is off at startup since the key is
                // repurposed for layout switching
                layout_indicator::ensure_caps_lock_off();

                if !is_background {
                    print_foreground_header(&config);
                }
            }
            Err(e) => {
                lifecycle_log::event(&format!("startup failed: keyboard hook: {e}"));
                if !is_background {
                    eprintln!("Hook installation error: {}", e);
                }
                cleanup_and_exit();
                return;
            }
        }

        // Ctrl+C handler for proper shutdown (only for foreground mode)
        if !is_background {
            // Try to set up Ctrl+C handler, but don't panic if it fails
            if let Err(e) = ctrlc::set_handler(move || {
                println!("\nShutting down...");
                cleanup_and_exit();
                std::process::exit(0);
            }) {
                println!("Warning: Could not set Ctrl+C handler: {}", e);
                println!("You may need to close the console window manually to exit.");
            }
        }

        // Switching runs on a worker thread that reports back to this one
        layout_switcher::start(GetCurrentThreadId(), config.win_space_fallback);

        // Create hidden window for message handling
        if create_message_window().is_null() {
            lifecycle_log::event(&format!(
                "startup failed: message window error {}",
                GetLastError()
            ));
            cleanup_and_exit();
            std::process::exit(1);
        }
        lifecycle_log::event("ready: keyboard hook and message loop installed");

        // Keep Scroll Lock in step with the foreground window's layout (timer on this thread)
        layout_indicator::start_indicator_sync();

        // Main message processing loop
        let mut msg: MSG = mem::zeroed();
        loop {
            let result = GetMessageW(&mut msg, ptr::null_mut(), 0, 0);
            if result == -1 {
                lifecycle_log::event(&format!(
                    "message loop failed: Windows error {}",
                    GetLastError()
                ));
                cleanup_and_exit();
                std::process::exit(1);
            }
            if result == 0 {
                lifecycle_log::event(&format!("message loop ended: WM_QUIT code={}", msg.wParam));
                break;
            }

            // Handle quit message
            if msg.message == WM_QUIT {
                break;
            }

            // A layout switch finished on the worker thread
            if msg.message == layout_switcher::WM_SWITCH_DONE {
                keyboard_hook::finish_switch(msg.wParam as u64, msg.lParam as usize);
                continue;
            }

            // Handle other system messages
            if session_end_confirmed(msg.message, msg.wParam) {
                lifecycle_log::event("session ended: confirmed by Windows");
                // System shutdown - cleanup and exit gracefully
                break;
            }

            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }

        // Cleanup
        cleanup_and_exit();
    }
}

// Header shown once when CCaps runs in the foreground: layouts, keys, log legend
fn print_foreground_header(config: &config::Config) {
    let layouts = keyboard_hook::selected_layouts();
    let current = layout_manager::get_current_layout()
        .and_then(|current| layouts.iter().position(|l| l.hkl == current.hkl));
    let layouts: Vec<(String, String)> = layouts
        .into_iter()
        .map(|l| (l.short_code, l.name))
        .collect();
    println!();
    println!(
        "{}",
        console_log::header(env!("CARGO_PKG_VERSION"), &layouts, current, config)
    );
}

// Simplified function to detach from console
unsafe fn detach_from_console() {
    // Unbind from the parent process console
    // Note: We don't hide the console window because the background process
    // is started with CREATE_NO_WINDOW flag, so it doesn't have a console window to hide
    FreeConsole();
}

// Helper function to cleanup resources
fn cleanup_and_exit() {
    unsafe {
        uninstall_hook();
        layout_switcher::stop();
        let mutex = MUTEX_HANDLE.swap(ptr::null_mut(), Ordering::SeqCst);
        if !mutex.is_null() {
            CloseHandle(mutex);
        }
    }
}

// Improved hidden window creation function
unsafe fn create_message_window() -> HWND {
    use winapi::um::libloaderapi::GetModuleHandleW;

    let class_name = "CCapsMessageWindow\0";
    let class_name_wide: Vec<u16> = OsString::from(class_name).encode_wide().collect();

    let window_name = "CCaps Layout Switcher\0";
    let window_name_wide: Vec<u16> = OsString::from(window_name).encode_wide().collect();

    // Custom window procedure for handling system messages
    unsafe extern "system" fn window_proc(
        hwnd: HWND,
        msg: UINT,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        match msg {
            WM_QUERYENDSESSION => {
                // This is only a question. A later WM_ENDSESSION may cancel shutdown.
                lifecycle_log::event("session end requested: remaining active until confirmed");
                1
            }
            WM_ENDSESSION => {
                if session_end_confirmed(msg, wparam) {
                    lifecycle_log::event("session end confirmed: requesting exit");
                    PostQuitMessage(0);
                } else {
                    lifecycle_log::event("session end cancelled: remaining active");
                }
                0
            }
            WM_DESTROY => {
                lifecycle_log::event("message window destroyed: requesting exit");
                PostQuitMessage(0);
                0
            }
            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    }

    // Register window class
    let wc = WNDCLASSW {
        style: 0,
        lpfnWndProc: Some(window_proc),
        cbClsExtra: 0,
        cbWndExtra: 0,
        hInstance: GetModuleHandleW(ptr::null()),
        hIcon: ptr::null_mut(),
        hCursor: ptr::null_mut(),
        hbrBackground: ptr::null_mut(),
        lpszMenuName: ptr::null(),
        lpszClassName: class_name_wide.as_ptr(),
    };

    RegisterClassW(&wc);

    // Create hidden window
    let hwnd = CreateWindowExW(
        0,
        class_name_wide.as_ptr(),
        window_name_wide.as_ptr(),
        0, // No window style (completely hidden)
        0,
        0,
        0,
        0,            // Position and size (irrelevant for hidden window)
        HWND_MESSAGE, // Message-only window (not displayed in UI)
        ptr::null_mut(),
        GetModuleHandleW(ptr::null()),
        ptr::null_mut(),
    );

    if hwnd.is_null() {
        // Fallback: try to create a regular hidden window
        CreateWindowExW(
            0,
            class_name_wide.as_ptr(),
            window_name_wide.as_ptr(),
            WS_OVERLAPPEDWINDOW,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            ptr::null_mut(),
            ptr::null_mut(),
            GetModuleHandleW(ptr::null()),
            ptr::null_mut(),
        )
    } else {
        hwnd
    }
}

fn session_end_confirmed(message: u32, session_ending: usize) -> bool {
    message == WM_ENDSESSION && session_ending != 0
}

#[cfg(test)]
mod lifecycle_tests {
    use super::*;

    #[test]
    fn session_query_and_cancel_do_not_request_exit() {
        assert!(!session_end_confirmed(WM_QUERYENDSESSION, 1));
        assert!(!session_end_confirmed(WM_ENDSESSION, 0));
        assert!(session_end_confirmed(WM_ENDSESSION, 1));
        assert!(!session_end_confirmed(WM_CLOSE, 1));
    }

    #[test]
    fn cancelled_session_request_keeps_message_window_alive() {
        unsafe {
            let window = create_message_window();
            assert!(!window.is_null());
            assert_eq!(SendMessageW(window, WM_QUERYENDSESSION, 1, 0), 1);
            SendMessageW(window, WM_ENDSESSION, 0, 0);
            let mut msg: MSG = mem::zeroed();
            let quit = PeekMessageW(&mut msg, ptr::null_mut(), WM_QUIT, WM_QUIT, PM_REMOVE);
            assert_eq!(quit, 0, "cancelled session must not post WM_QUIT");
            assert_ne!(IsWindow(window), 0);
            DestroyWindow(window);
            // Discard the normal teardown quit message on this test thread.
            PeekMessageW(&mut msg, ptr::null_mut(), WM_QUIT, WM_QUIT, PM_REMOVE);
        }
    }
}
