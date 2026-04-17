//! Windows WebView2 implementation.

use std::ffi::c_void;
use std::ptr;
use std::sync::mpsc;

use crate::error::{Result, WebViewError};
use crate::mime::mime_for_path;
use crate::WebViewConfig;
use webview2_com::pwstr::take_pwstr;
use webview2_com::{
        AddScriptToExecuteOnDocumentCreatedCompletedHandler,
        CreateCoreWebView2ControllerCompletedHandler,
        CreateCoreWebView2EnvironmentCompletedHandler,
        ExecuteScriptCompletedHandler,
        NavigationCompletedEventHandler,
        WebMessageReceivedEventHandler,
        WebResourceRequestedEventHandler,
        CoTaskMemPWSTR,
};
use webview2_com::Microsoft::Web::WebView2::Win32::{
        CreateCoreWebView2Environment,
        ICoreWebView2,
        ICoreWebView2Controller,
        ICoreWebView2Controller2,
        ICoreWebView2Environment,
        COREWEBVIEW2_COLOR,
        COREWEBVIEW2_WEB_RESOURCE_CONTEXT_ALL,
};
use windows::core::{BOOL, Error as WindowsError, E_POINTER, Interface, PWSTR};
use windows::Win32::Foundation::{HWND, RECT};
use windows::Win32::System::Com::{CoInitializeEx, IStream, COINIT_APARTMENTTHREADED};
use windows::Win32::UI::Shell::SHCreateMemStream;
use windows::Win32::UI::WindowsAndMessaging::GetClientRect;

/// Inject a minimal WebKit-compatible message handler so the shared Beamer
/// runtime can run unchanged on WebView2.
const WINDOWS_IPC_COMPAT_JS: &str = r#"(function(){
    if (!window.chrome || !window.chrome.webview) return;
    var webkit = window.webkit || {};
    var handlers = webkit.messageHandlers || {};
    handlers.beamer = {
        postMessage: function(x) { window.chrome.webview.postMessage(x); }
    };
    webkit.messageHandlers = handlers;
    window.webkit = webkit;
})();"#;

/// Shared Beamer runtime injected at document start.
const BEAMER_RUNTIME_JS: &str = include_str!("beamer_runtime.js");

const ASSET_BASE_URL: &str = "http://beamer.localhost";
const ASSET_INDEX_URL: &str = "http://beamer.localhost/index.html";

/// Windows WebView backed by WebView2.
pub struct WindowsWebView {
        controller: ICoreWebView2Controller,
        webview: ICoreWebView2,
        navigation_completed_token: Option<i64>,
        web_message_token: Option<i64>,
        web_resource_requested_token: Option<i64>,
}

impl WindowsWebView {
    /// Attach a WebView2 to the given parent HWND.
    ///
    /// # Safety
    ///
    /// `parent` must be a valid `HWND` provided by the VST3 host.
    pub unsafe fn attach_to_parent(
        parent: *mut c_void,
        config: &WebViewConfig<'_>,
    ) -> Result<Self> {
        if parent.is_null() {
            return Err(WebViewError::CreationFailed("null parent HWND".into()));
        }

        initialize_com()?;

        let parent = HWND(parent);
        let environment = create_environment()?;
        let controller = create_controller(&environment, parent)?;
        let webview = unsafe { controller.CoreWebView2() }.map_err(webview_error)?;

        configure_controller(parent, &controller, config)?;
        configure_settings(&webview, config)?;

        let mut navigation_completed_token = None;
        let mut web_message_token = None;
        let mut web_resource_requested_token = None;

        if config.message_callback.is_some() {
            add_init_script(
                &webview,
                format!("{WINDOWS_IPC_COMPAT_JS}\n{BEAMER_RUNTIME_JS}"),
            )?;

            let callback = config.message_callback;
            let context = config.callback_context;
            let mut token = 0;
            unsafe {
                webview.add_WebMessageReceived(
                    &WebMessageReceivedEventHandler::create(Box::new(move |_, args| {
                        let (Some(args), Some(callback)) = (args, callback) else {
                            return Ok(());
                        };

                        let mut message = PWSTR::null();
                        args.TryGetWebMessageAsString(&mut message)?;
                        let message = take_pwstr(message);
                        callback(context, message.as_ptr(), message.len());
                        Ok(())
                    })),
                    &mut token,
                )
            }
            .map_err(webview_error)?;
            web_message_token = Some(token);
        }

        if let Some(loaded_callback) = config.loaded_callback {
            let context = config.callback_context;
            let mut token = 0;
            unsafe {
                webview.add_NavigationCompleted(
                    &NavigationCompletedEventHandler::create(Box::new(move |_, args| {
                        let Some(args) = args else {
                            return Ok(());
                        };

                        let mut success = BOOL::default();
                        args.IsSuccess(&mut success)?;
                        if success.as_bool() {
                            loaded_callback(context);
                        }
                        Ok(())
                    })),
                    &mut token,
                )
            }
            .map_err(webview_error)?;
            navigation_completed_token = Some(token);
        }

        if let Some(assets) = config.assets {
            let filter = format!("{ASSET_BASE_URL}/*");
            let filter = CoTaskMemPWSTR::from(filter.as_str());
            unsafe {
                webview.AddWebResourceRequestedFilter(
                    *filter.as_ref().as_pcwstr(),
                    COREWEBVIEW2_WEB_RESOURCE_CONTEXT_ALL,
                )
            }
            .map_err(webview_error)?;

            let environment = environment.clone();
            let mut token = 0;
            unsafe {
                webview.add_WebResourceRequested(
                    &WebResourceRequestedEventHandler::create(Box::new(move |_, args| {
                        let Some(args) = args else {
                            return Ok(());
                        };

                        let request = args.Request()?;
                        let mut uri = PWSTR::null();
                        request.Uri(&mut uri)?;
                        let uri = take_pwstr(uri);

                        let response = match asset_path_from_request(&uri) {
                            Some(path) => {
                                if let Some(bytes) = assets.get(path) {
                                    create_response(
                                        &environment,
                                        200,
                                        "OK",
                                        &format!("Content-Type: {}\r\n", mime_for_path(path)),
                                        Some(bytes),
                                    )?
                                } else {
                                    create_response(
                                        &environment,
                                        404,
                                        "Not Found",
                                        "Content-Type: text/plain\r\n",
                                        Some(b"Not Found"),
                                    )?
                                }
                            }
                            None => return Ok(()),
                        };

                        args.SetResponse(&response)?;
                        Ok(())
                    })),
                    &mut token,
                )
            }
            .map_err(webview_error)?;
            web_resource_requested_token = Some(token);
        }

        let initial_url = if config.url.is_some() {
            config.url
        } else if config.assets.is_some() {
            Some(ASSET_INDEX_URL)
        } else {
            None
        };

        if let Some(url) = initial_url {
            let url = CoTaskMemPWSTR::from(url);
            unsafe { webview.Navigate(*url.as_ref().as_pcwstr()) }.map_err(webview_error)?;
        }

        Ok(Self {
            controller,
            webview,
            navigation_completed_token,
            web_message_token,
            web_resource_requested_token,
        })
    }

    /// Update the WebView bounds.
    pub fn set_bounds(&self, x: i32, y: i32, width: i32, height: i32) {
        let bounds = RECT {
            left: x,
            top: y,
            right: x.saturating_add(width.max(0)),
            bottom: y.saturating_add(height.max(0)),
        };
        if let Err(err) = unsafe { self.controller.SetBounds(bounds) } {
            log::warn!("failed to resize WebView2: {err}");
        }
    }

    /// Execute JavaScript in the WebView.
    pub fn evaluate_js(&self, js: &str) {
        let script = CoTaskMemPWSTR::from(js);
        let handler = ExecuteScriptCompletedHandler::create(Box::new(|_, _| Ok(())));
        if let Err(err) = unsafe {
            self.webview.ExecuteScript(*script.as_ref().as_pcwstr(), &handler)
        } {
            log::warn!("failed to evaluate JavaScript in WebView2: {err}");
        }
    }

    /// Remove the WebView from its parent.
    pub fn detach(&mut self) {
        if let Some(token) = self.web_resource_requested_token.take() {
            let _ = unsafe { self.webview.remove_WebResourceRequested(token) };
        }
        if let Some(token) = self.web_message_token.take() {
            let _ = unsafe { self.webview.remove_WebMessageReceived(token) };
        }
        if let Some(token) = self.navigation_completed_token.take() {
            let _ = unsafe { self.webview.remove_NavigationCompleted(token) };
        }
        let _ = unsafe { self.controller.Close() };
    }
}

fn initialize_com() -> Result<()> {
    match unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) } {
        Ok(()) => Ok(()),
        Err(err) if err.code().0 >= 0 => Ok(()),
        Err(err) => Err(webview_error(err)),
    }
}

fn create_environment() -> Result<ICoreWebView2Environment> {
    let (tx, rx) = mpsc::channel();
    CreateCoreWebView2EnvironmentCompletedHandler::wait_for_async_operation(
        Box::new(|handler| unsafe {
            CreateCoreWebView2Environment(&handler).map_err(webview2_error)
        }),
        Box::new(move |error_code, environment| {
            error_code?;
            tx.send(environment.ok_or_else(|| WindowsError::from(E_POINTER)))
                .map_err(|_| webview2_com::Error::SendError)?;
            Ok(())
        }),
    )
    .map_err(webview_error)?;

    rx.recv()
        .map_err(|_| WebViewError::CreationFailed("failed to receive WebView2 environment".into()))?
        .map_err(webview_error)
}

fn create_controller(
    environment: &ICoreWebView2Environment,
    parent: HWND,
) -> Result<ICoreWebView2Controller> {
    let (tx, rx) = mpsc::channel();
    let environment = environment.clone();
    CreateCoreWebView2ControllerCompletedHandler::wait_for_async_operation(
        Box::new(move |handler| unsafe {
            environment
                .CreateCoreWebView2Controller(parent, &handler)
                .map_err(webview2_error)
        }),
        Box::new(move |error_code, controller| {
            error_code?;
            tx.send(controller.ok_or_else(|| WindowsError::from(E_POINTER)))
                .map_err(|_| webview2_com::Error::SendError)?;
            Ok(())
        }),
    )
    .map_err(webview_error)?;

    rx.recv()
        .map_err(|_| WebViewError::CreationFailed("failed to receive WebView2 controller".into()))?
        .map_err(webview_error)
}

fn configure_controller(
    parent: HWND,
    controller: &ICoreWebView2Controller,
    config: &WebViewConfig<'_>,
) -> Result<()> {
    let mut bounds = RECT::default();
    unsafe { GetClientRect(parent, &mut bounds) }.map_err(webview_error)?;
    unsafe { controller.SetBounds(bounds) }.map_err(webview_error)?;
    unsafe { controller.SetIsVisible(true) }.map_err(webview_error)?;

    if config.background_color != [0, 0, 0, 0] {
        if let Ok(controller2) = controller.cast::<ICoreWebView2Controller2>() {
            let [r, g, b, a] = config.background_color;
            let color = COREWEBVIEW2_COLOR { A: a, R: r, G: g, B: b };
            unsafe { controller2.SetDefaultBackgroundColor(&color) }.map_err(webview_error)?;
        }
    }

    Ok(())
}

fn configure_settings(webview: &ICoreWebView2, config: &WebViewConfig<'_>) -> Result<()> {
    let settings = unsafe { webview.Settings() }.map_err(webview_error)?;
    unsafe { settings.SetIsWebMessageEnabled(config.message_callback.is_some()) }
        .map_err(webview_error)?;
    unsafe { settings.SetAreDevToolsEnabled(config.dev_tools) }.map_err(webview_error)?;
    Ok(())
}

fn add_init_script(webview: &ICoreWebView2, script: String) -> Result<()> {
    let webview = webview.clone();
    AddScriptToExecuteOnDocumentCreatedCompletedHandler::wait_for_async_operation(
        Box::new(move |handler| unsafe {
            let script = CoTaskMemPWSTR::from(script.as_str());
            webview
                .AddScriptToExecuteOnDocumentCreated(*script.as_ref().as_pcwstr(), &handler)
                .map_err(webview2_error)
        }),
        Box::new(|error_code, _| error_code),
    )
    .map_err(webview_error)
}

fn asset_path_from_request(uri: &str) -> Option<&str> {
    let path = uri.strip_prefix(ASSET_BASE_URL)?;
    if path.is_empty() || path == "/" {
        Some("index.html")
    } else {
        Some(path.trim_start_matches('/'))
    }
}

fn create_response(
    environment: &ICoreWebView2Environment,
    status_code: i32,
    reason_phrase: &str,
    headers: &str,
    body: Option<&'static [u8]>,
) -> windows::core::Result<webview2_com::Microsoft::Web::WebView2::Win32::ICoreWebView2WebResourceResponse> {
    let stream: Option<IStream> = match body {
        Some(bytes) if !bytes.is_empty() => unsafe { SHCreateMemStream(Some(bytes)) },
        _ => None,
    };
    let reason = CoTaskMemPWSTR::from(reason_phrase);
    let headers = CoTaskMemPWSTR::from(headers);
    unsafe {
        environment.CreateWebResourceResponse(
            stream,
            status_code,
            *reason.as_ref().as_pcwstr(),
            *headers.as_ref().as_pcwstr(),
        )
    }
}

fn webview2_error(err: WindowsError) -> webview2_com::Error {
    webview2_com::Error::WindowsError(err)
}

fn webview_error<E: std::fmt::Display>(err: E) -> WebViewError {
    WebViewError::CreationFailed(err.to_string())
}
