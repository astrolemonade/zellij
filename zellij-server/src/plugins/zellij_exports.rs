use super::PluginInstruction;
use crate::plugins::plugin_map::{PluginEnv, Subscriptions};
use crate::plugins::wasm_bridge::handle_plugin_crash;
use crate::route::route_action;
use log::{debug, warn};
use serde::Serialize;
use std::{
    collections::HashSet,
    path::PathBuf,
    process,
    sync::{Arc, Mutex},
    thread,
    time::{Duration, Instant},
};
use wasmer::{imports, Function, ImportObject, Store, WasmerEnv};
use wasmer_wasi::WasiEnv;

use url::Url;

use crate::{panes::PaneId, screen::ScreenInstruction};

use zellij_utils::{
    consts::VERSION,
    data::{
        CommandToRun, Direction, Event, EventType, FileToOpen, InputMode, PluginCommand, PluginIds,
        PluginMessage, Resize, ResizeStrategy,
    },
    errors::prelude::*,
    input::{
        actions::Action,
        command::{RunCommand, RunCommandAction, TerminalAction},
        layout::{Layout, RunPlugin, RunPluginLocation},
        plugins::PluginType,
    },
    plugin_api::{
        plugin_command::ProtobufPluginCommand,
        plugin_ids::{ProtobufPluginIds, ProtobufZellijVersion},
    },
    prost::Message,
    serde,
};

macro_rules! apply_action {
    ($action:ident, $error_message:ident, $env: ident) => {
        if let Err(e) = route_action(
            $action,
            $env.plugin_env.client_id,
            $env.plugin_env.senders.clone(),
            $env.plugin_env.capabilities.clone(),
            $env.plugin_env.client_attributes.clone(),
            $env.plugin_env.default_shell.clone(),
            $env.plugin_env.default_layout.clone(),
        ) {
            log::error!("{}: {:?}", $error_message(), e);
        }
    };
}

pub fn zellij_exports(
    store: &Store,
    plugin_env: &PluginEnv,
    subscriptions: &Arc<Mutex<Subscriptions>>,
) -> ImportObject {
    imports! {
        "zellij" => {
          "host_run_plugin_command" => {
            Function::new_native_with_env(store, ForeignFunctionEnv::new(plugin_env, subscriptions), host_run_plugin_command)
          }
        }
    }
}

#[derive(WasmerEnv, Clone)]
pub struct ForeignFunctionEnv {
    pub plugin_env: PluginEnv,
    pub subscriptions: Arc<Mutex<Subscriptions>>,
}

impl ForeignFunctionEnv {
    pub fn new(plugin_env: &PluginEnv, subscriptions: &Arc<Mutex<Subscriptions>>) -> Self {
        ForeignFunctionEnv {
            plugin_env: plugin_env.clone(),
            subscriptions: subscriptions.clone(),
        }
    }
}

fn host_run_plugin_command(env: &ForeignFunctionEnv) {
    wasi_read_bytes(&env.plugin_env.wasi_env)
        .and_then(|bytes| {
            let command: ProtobufPluginCommand = ProtobufPluginCommand::decode(bytes.as_slice())?;
            let command: PluginCommand = command
                .try_into()
                .map_err(|e| anyhow!("failed to convert serialized command: {}", e))?;
            match command {
                PluginCommand::Subscribe(event_list) => subscribe(env, event_list)?,
                PluginCommand::Unsubscribe(event_list) => unsubscribe(env, event_list)?,
                PluginCommand::SetSelectable(selectable) => set_selectable(env, selectable),
                PluginCommand::GetPluginIds => get_plugin_ids(env),
                PluginCommand::GetZellijVersion => get_zellij_version(env),
                PluginCommand::OpenFile(file_to_open) => open_file(env, file_to_open),
                PluginCommand::OpenFileFloating(file_to_open) => {
                    open_file_floating(env, file_to_open)
                },
                PluginCommand::OpenTerminal(cwd) => open_terminal(env, cwd.path.try_into()?),
                PluginCommand::OpenTerminalFloating(cwd) => {
                    open_terminal_floating(env, cwd.path.try_into()?)
                },
                PluginCommand::OpenCommandPane(command_to_run) => {
                    open_command_pane(env, command_to_run)
                },
                PluginCommand::OpenCommandPaneFloating(command_to_run) => {
                    open_command_pane_floating(env, command_to_run)
                },
                PluginCommand::SwitchTabTo(tab_index) => switch_tab_to(env, tab_index),
                PluginCommand::SetTimeout(seconds) => set_timeout(env, seconds),
                PluginCommand::ExecCmd(command_line) => exec_cmd(env, command_line),
                PluginCommand::PostMessageTo(plugin_message) => {
                    post_message_to(env, plugin_message)?
                },
                PluginCommand::PostMessageToPlugin(plugin_message) => {
                    post_message_to_plugin(env, plugin_message)?
                },
                PluginCommand::HideSelf => hide_self(env)?,
                PluginCommand::ShowSelf(should_float_if_hidden) => {
                    show_self(env, should_float_if_hidden)
                },
                PluginCommand::SwitchToMode(input_mode) => {
                    switch_to_mode(env, input_mode.try_into()?)
                },
                PluginCommand::NewTabsWithLayout(raw_layout) => {
                    new_tabs_with_layout(env, &raw_layout)?
                },
                PluginCommand::NewTab => new_tab(env),
                PluginCommand::GoToNextTab => go_to_next_tab(env),
                PluginCommand::GoToPreviousTab => go_to_previous_tab(env),
                PluginCommand::Resize(resize_payload) => resize(env, resize_payload),
                PluginCommand::ResizeWithDirection(resize_strategy) => {
                    resize_with_direction(env, resize_strategy)
                },
                PluginCommand::FocusNextPane => focus_next_pane(env),
                PluginCommand::FocusPreviousPane => focus_previous_pane(env),
                PluginCommand::MoveFocus(direction) => move_focus(env, direction),
                PluginCommand::MoveFocusOrTab(direction) => move_focus_or_tab(env, direction),
                PluginCommand::Detach => detach(env),
                PluginCommand::EditScrollback => edit_scrollback(env),
                PluginCommand::Write(bytes) => write(env, bytes),
                PluginCommand::WriteChars(chars) => write_chars(env, chars),
                PluginCommand::ToggleTab => toggle_tab(env),
                PluginCommand::MovePane => move_pane(env),
                PluginCommand::MovePaneWithDirection(direction) => {
                    move_pane_with_direction(env, direction)
                },
                PluginCommand::ClearScreen => clear_screen(env),
                PluginCommand::ScrollUp => scroll_up(env),
                PluginCommand::ScrollDown => scroll_down(env),
                PluginCommand::ScrollToTop => scroll_to_top(env),
                PluginCommand::ScrollToBottom => scroll_to_bottom(env),
                PluginCommand::PageScrollUp => page_scroll_up(env),
                PluginCommand::PageScrollDown => page_scroll_down(env),
                PluginCommand::ToggleFocusFullscreen => toggle_focus_fullscreen(env),
                PluginCommand::TogglePaneFrames => toggle_pane_frames(env),
                PluginCommand::TogglePaneEmbedOrEject => toggle_pane_embed_or_eject(env),
                PluginCommand::UndoRenamePane => undo_rename_pane(env),
                PluginCommand::CloseFocus => close_focus(env),
                PluginCommand::ToggleActiveTabSync => toggle_active_tab_sync(env),
                PluginCommand::CloseFocusedTab => close_focused_tab(env),
                PluginCommand::UndoRenameTab => undo_rename_tab(env),
                PluginCommand::QuitZellij => quit_zellij(env),
                PluginCommand::PreviousSwapLayout => previous_swap_layout(env),
                PluginCommand::NextSwapLayout => next_swap_layout(env),
                PluginCommand::GoToTabName(tab_name) => go_to_tab_name(env, tab_name),
                PluginCommand::FocusOrCreateTab(tab_name) => focus_or_create_tab(env, tab_name),
                PluginCommand::GoToTab(tab_index) => go_to_tab(env, tab_index),
                PluginCommand::StartOrReloadPlugin(plugin_url) => {
                    start_or_reload_plugin(env, &plugin_url)?
                },
                PluginCommand::CloseTerminalPane(terminal_pane_id) => {
                    close_terminal_pane(env, terminal_pane_id)
                },
                PluginCommand::ClosePluginPane(plugin_pane_id) => {
                    close_plugin_pane(env, plugin_pane_id)
                },
                PluginCommand::FocusTerminalPane(terminal_pane_id, should_float_if_hidden) => {
                    focus_terminal_pane(env, terminal_pane_id, should_float_if_hidden)
                },
                PluginCommand::FocusPluginPane(plugin_pane_id, should_float_if_hidden) => {
                    focus_plugin_pane(env, plugin_pane_id, should_float_if_hidden)
                },
                PluginCommand::RenameTerminalPane(terminal_pane_id, new_name) => {
                    rename_terminal_pane(env, terminal_pane_id, &new_name)
                },
                PluginCommand::RenamePluginPane(plugin_pane_id, new_name) => {
                    rename_plugin_pane(env, plugin_pane_id, &new_name)
                },
                PluginCommand::RenameTab(tab_index, new_name) => {
                    rename_tab(env, tab_index, &new_name)
                },
                PluginCommand::ReportPanic(crash_payload) => report_panic(env, &crash_payload),
            }
            Ok(())
        })
        .with_context(|| format!("failed to run plugin command {}", env.plugin_env.name()))
        .non_fatal();
}

fn subscribe(env: &ForeignFunctionEnv, event_list: HashSet<EventType>) -> Result<()> {
    env.subscriptions
        .lock()
        .to_anyhow()?
        .extend(event_list.clone());
    env.plugin_env
        .senders
        .send_to_plugin(PluginInstruction::PluginSubscribedToEvents(
            env.plugin_env.plugin_id,
            env.plugin_env.client_id,
            event_list,
        ))
}

fn unsubscribe(env: &ForeignFunctionEnv, event_list: HashSet<EventType>) -> Result<()> {
    env.subscriptions
        .lock()
        .to_anyhow()?
        .retain(|k| !event_list.contains(k));
    Ok(())
}

fn set_selectable(env: &ForeignFunctionEnv, selectable: bool) {
    match env.plugin_env.plugin.run {
        PluginType::Pane(Some(tab_index)) => {
            // let selectable = selectable != 0;
            env.plugin_env
                .senders
                .send_to_screen(ScreenInstruction::SetSelectable(
                    PaneId::Plugin(env.plugin_env.plugin_id),
                    selectable,
                    tab_index,
                ))
                .with_context(|| {
                    format!(
                        "failed to set plugin {} selectable from plugin {}",
                        selectable,
                        env.plugin_env.name()
                    )
                })
                .non_fatal();
        },
        _ => {
            debug!(
                "{} - Calling method 'set_selectable' does nothing for headless plugins",
                env.plugin_env.plugin.location
            )
        },
    }
}

fn get_plugin_ids(env: &ForeignFunctionEnv) {
    let ids = PluginIds {
        plugin_id: env.plugin_env.plugin_id,
        zellij_pid: process::id(),
    };
    ProtobufPluginIds::try_from(ids)
        .map_err(|e| anyhow!("Failed to serialized plugin ids: {}", e))
        .and_then(|serialized| {
            wasi_write_object(&env.plugin_env.wasi_env, &serialized.encode_to_vec())?;
            Ok(())
        })
        .with_context(|| {
            format!(
                "failed to query plugin IDs from host for plugin {}",
                env.plugin_env.name()
            )
        })
        .non_fatal();
}

fn get_zellij_version(env: &ForeignFunctionEnv) {
    let protobuf_zellij_version = ProtobufZellijVersion {
        version: VERSION.to_owned(),
    };
    wasi_write_object(
        &env.plugin_env.wasi_env,
        &protobuf_zellij_version.encode_to_vec(),
    )
    .with_context(|| {
        format!(
            "failed to request zellij version from host for plugin {}",
            env.plugin_env.name()
        )
    })
    .non_fatal();
}

fn open_file(env: &ForeignFunctionEnv, file_to_open: FileToOpen) {
    let error_msg = || format!("failed to open file in plugin {}", env.plugin_env.name());
    let floating = false;
    let action = Action::EditFile(
        file_to_open.path,
        file_to_open.line_number,
        file_to_open.cwd,
        None,
        floating,
    );
    apply_action!(action, error_msg, env);
}

fn open_file_floating(env: &ForeignFunctionEnv, file_to_open: FileToOpen) {
    let error_msg = || format!("failed to open file in plugin {}", env.plugin_env.name());
    let floating = true;
    let action = Action::EditFile(
        file_to_open.path,
        file_to_open.line_number,
        file_to_open.cwd,
        None,
        floating,
    );
    apply_action!(action, error_msg, env);
}

fn open_terminal(env: &ForeignFunctionEnv, cwd: PathBuf) {
    let error_msg = || format!("failed to open file in plugin {}", env.plugin_env.name());
    let mut default_shell = env
        .plugin_env
        .default_shell
        .clone()
        .unwrap_or_else(|| TerminalAction::RunCommand(RunCommand::default()));
    default_shell.change_cwd(cwd);
    let run_command_action: Option<RunCommandAction> = match default_shell {
        TerminalAction::RunCommand(run_command) => Some(run_command.into()),
        _ => None,
    };
    let action = Action::NewTiledPane(None, run_command_action, None);
    apply_action!(action, error_msg, env);
}

fn open_terminal_floating(env: &ForeignFunctionEnv, cwd: PathBuf) {
    let error_msg = || format!("failed to open file in plugin {}", env.plugin_env.name());
    let mut default_shell = env
        .plugin_env
        .default_shell
        .clone()
        .unwrap_or_else(|| TerminalAction::RunCommand(RunCommand::default()));
    default_shell.change_cwd(cwd);
    let run_command_action: Option<RunCommandAction> = match default_shell {
        TerminalAction::RunCommand(run_command) => Some(run_command.into()),
        _ => None,
    };
    let action = Action::NewFloatingPane(run_command_action, None);
    apply_action!(action, error_msg, env);
}

fn open_command_pane(env: &ForeignFunctionEnv, command_to_run: CommandToRun) {
    let error_msg = || format!("failed to open command in plugin {}", env.plugin_env.name());
    let command = command_to_run.path;
    let cwd = command_to_run.cwd;
    let args = command_to_run.args;
    let direction = None;
    let hold_on_close = true;
    let hold_on_start = false;
    let name = None;
    let run_command_action = RunCommandAction {
        command,
        args,
        cwd,
        direction,
        hold_on_close,
        hold_on_start,
    };
    let action = Action::NewTiledPane(direction, Some(run_command_action), name);
    apply_action!(action, error_msg, env);
}

fn open_command_pane_floating(env: &ForeignFunctionEnv, command_to_run: CommandToRun) {
    let error_msg = || format!("failed to open command in plugin {}", env.plugin_env.name());
    let command = command_to_run.path;
    let cwd = command_to_run.cwd;
    let args = command_to_run.args;
    let direction = None;
    let hold_on_close = true;
    let hold_on_start = false;
    let name = None;
    let run_command_action = RunCommandAction {
        command,
        args,
        cwd,
        direction,
        hold_on_close,
        hold_on_start,
    };
    let action = Action::NewFloatingPane(Some(run_command_action), name);
    apply_action!(action, error_msg, env);
}

fn switch_tab_to(env: &ForeignFunctionEnv, tab_idx: u32) {
    env.plugin_env
        .senders
        .send_to_screen(ScreenInstruction::GoToTab(
            tab_idx,
            Some(env.plugin_env.client_id),
        ))
        .with_context(|| {
            format!(
                "failed to switch to tab {tab_idx} from plugin {}",
                env.plugin_env.name()
            )
        })
        .non_fatal();
}

fn set_timeout(env: &ForeignFunctionEnv, secs: f32) {
    // There is a fancy, high-performance way to do this with zero additional threads:
    // If the plugin thread keeps a BinaryHeap of timer structs, it can manage multiple and easily `.peek()` at the
    // next time to trigger in O(1) time. Once the wake-up time is known, the `wasm` thread can use `recv_timeout()`
    // to wait for an event with the timeout set to be the time of the next wake up. If events come in in the meantime,
    // they are handled, but if the timeout triggers, we replace the event from `recv()` with an
    // `Update(pid, TimerEvent)` and pop the timer from the Heap (or reschedule it). No additional threads for as many
    // timers as we'd like.
    //
    // But that's a lot of code, and this is a few lines:
    let send_plugin_instructions = env.plugin_env.senders.to_plugin.clone();
    let update_target = Some(env.plugin_env.plugin_id);
    let client_id = env.plugin_env.client_id;
    let plugin_name = env.plugin_env.name();
    // TODO: we should really use an async task for this
    thread::spawn(move || {
        let start_time = Instant::now();
        thread::sleep(Duration::from_secs_f32(secs));
        // FIXME: The way that elapsed time is being calculated here is not exact; it doesn't take into account the
        // time it takes an event to actually reach the plugin after it's sent to the `wasm` thread.
        let elapsed_time = Instant::now().duration_since(start_time).as_secs_f64();

        send_plugin_instructions
            .ok_or(anyhow!("found no sender to send plugin instruction to"))
            .and_then(|sender| {
                sender
                    .send(PluginInstruction::Update(vec![(
                        update_target,
                        Some(client_id),
                        Event::Timer(elapsed_time),
                    )]))
                    .to_anyhow()
            })
            .with_context(|| {
                format!(
                    "failed to set host timeout of {secs} s for plugin {}",
                    plugin_name
                )
            })
            .non_fatal();
    });
}

fn exec_cmd(env: &ForeignFunctionEnv, mut command_line: Vec<String>) {
    let err_context = || {
        format!(
            "failed to execute command on host for plugin '{}'",
            env.plugin_env.name()
        )
    };
    let command = command_line.remove(0);

    // Bail out if we're forbidden to run command
    if !env.plugin_env.plugin._allow_exec_host_cmd {
        warn!("This plugin isn't allow to run command in host side, skip running this command: '{cmd} {args}'.",
        	cmd = command, args = command_line.join(" "));
        return;
    }

    // Here, we don't wait the command to finish
    process::Command::new(command)
        .args(command_line)
        .spawn()
        .with_context(err_context)
        .non_fatal();
}

fn post_message_to(env: &ForeignFunctionEnv, plugin_message: PluginMessage) -> Result<()> {
    let worker_name = plugin_message
        .worker_name
        .ok_or(anyhow!("Worker name not specified in message to worker"))?;
    env.plugin_env
        .senders
        .send_to_plugin(PluginInstruction::PostMessagesToPluginWorker(
            env.plugin_env.plugin_id,
            env.plugin_env.client_id,
            worker_name,
            vec![(plugin_message.name, plugin_message.payload)],
        ))
}

fn post_message_to_plugin(env: &ForeignFunctionEnv, plugin_message: PluginMessage) -> Result<()> {
    if let Some(worker_name) = plugin_message.worker_name {
        return Err(anyhow!(
            "Worker name (\"{}\") should not be specified in message to plugin",
            worker_name
        ));
    }
    env.plugin_env
        .senders
        .send_to_plugin(PluginInstruction::PostMessageToPlugin(
            env.plugin_env.plugin_id,
            env.plugin_env.client_id,
            plugin_message.name,
            plugin_message.payload,
        ))
}

fn hide_self(env: &ForeignFunctionEnv) -> Result<()> {
    env.plugin_env
        .senders
        .send_to_screen(ScreenInstruction::SuppressPane(
            PaneId::Plugin(env.plugin_env.plugin_id),
            env.plugin_env.client_id,
        ))
        .with_context(|| format!("failed to hide self"))
}

fn show_self(env: &ForeignFunctionEnv, should_float_if_hidden: bool) {
    let action = Action::FocusPluginPaneWithId(env.plugin_env.plugin_id, should_float_if_hidden);
    let error_msg = || format!("Failed to show self for plugin");
    apply_action!(action, error_msg, env);
}

fn switch_to_mode(env: &ForeignFunctionEnv, input_mode: InputMode) {
    let action = Action::SwitchToMode(input_mode);
    let error_msg = || {
        format!(
            "failed to switch to mode in plugin {}",
            env.plugin_env.name()
        )
    };
    apply_action!(action, error_msg, env);
}

fn new_tabs_with_layout(env: &ForeignFunctionEnv, raw_layout: &str) -> Result<()> {
    // TODO: cwd
    let layout = Layout::from_str(
        &raw_layout,
        format!("Layout from plugin: {}", env.plugin_env.name()),
        None,
        None,
    )
    .map_err(|e| anyhow!("Failed to parse layout: {:?}", e))?;
    let mut tabs_to_open = vec![];
    let tabs = layout.tabs();
    if tabs.is_empty() {
        let swap_tiled_layouts = Some(layout.swap_tiled_layouts.clone());
        let swap_floating_layouts = Some(layout.swap_floating_layouts.clone());
        let action = Action::NewTab(
            layout.template.as_ref().map(|t| t.0.clone()),
            layout.template.map(|t| t.1).unwrap_or_default(),
            swap_tiled_layouts,
            swap_floating_layouts,
            None,
        );
        tabs_to_open.push(action);
    } else {
        for (tab_name, tiled_pane_layout, floating_pane_layout) in layout.tabs() {
            let swap_tiled_layouts = Some(layout.swap_tiled_layouts.clone());
            let swap_floating_layouts = Some(layout.swap_floating_layouts.clone());
            let action = Action::NewTab(
                Some(tiled_pane_layout),
                floating_pane_layout,
                swap_tiled_layouts,
                swap_floating_layouts,
                tab_name,
            );
            tabs_to_open.push(action);
        }
    }
    for action in tabs_to_open {
        let error_msg = || format!("Failed to create layout tab");
        apply_action!(action, error_msg, env);
    }
    Ok(())
}

fn new_tab(env: &ForeignFunctionEnv) {
    let action = Action::NewTab(None, vec![], None, None, None);
    let error_msg = || format!("Failed to open new tab");
    apply_action!(action, error_msg, env);
}

fn go_to_next_tab(env: &ForeignFunctionEnv) {
    let action = Action::GoToNextTab;
    let error_msg = || format!("Failed to go to next tab");
    apply_action!(action, error_msg, env);
}

fn go_to_previous_tab(env: &ForeignFunctionEnv) {
    let action = Action::GoToPreviousTab;
    let error_msg = || format!("Failed to go to previous tab");
    apply_action!(action, error_msg, env);
}

fn resize(env: &ForeignFunctionEnv, resize: Resize) {
    let error_msg = || format!("failed to resize in plugin {}", env.plugin_env.name());
    let action = Action::Resize(resize, None);
    apply_action!(action, error_msg, env);
}

fn resize_with_direction(env: &ForeignFunctionEnv, resize: ResizeStrategy) {
    let error_msg = || format!("failed to resize in plugin {}", env.plugin_env.name());
    let action = Action::Resize(resize.resize, resize.direction);
    apply_action!(action, error_msg, env);
}

fn focus_next_pane(env: &ForeignFunctionEnv) {
    let action = Action::FocusNextPane;
    let error_msg = || format!("Failed to focus next pane");
    apply_action!(action, error_msg, env);
}

fn focus_previous_pane(env: &ForeignFunctionEnv) {
    let action = Action::FocusPreviousPane;
    let error_msg = || format!("Failed to focus previous pane");
    apply_action!(action, error_msg, env);
}

fn move_focus(env: &ForeignFunctionEnv, direction: Direction) {
    let error_msg = || format!("failed to move focus in plugin {}", env.plugin_env.name());
    let action = Action::MoveFocus(direction);
    apply_action!(action, error_msg, env);
}

fn move_focus_or_tab(env: &ForeignFunctionEnv, direction: Direction) {
    let error_msg = || format!("failed to move focus in plugin {}", env.plugin_env.name());
    let action = Action::MoveFocusOrTab(direction);
    apply_action!(action, error_msg, env);
}

fn detach(env: &ForeignFunctionEnv) {
    let action = Action::Detach;
    let error_msg = || format!("Failed to detach");
    apply_action!(action, error_msg, env);
}

fn edit_scrollback(env: &ForeignFunctionEnv) {
    let action = Action::EditScrollback;
    let error_msg = || format!("Failed to edit scrollback");
    apply_action!(action, error_msg, env);
}

fn write(env: &ForeignFunctionEnv, bytes: Vec<u8>) {
    let error_msg = || format!("failed to write in plugin {}", env.plugin_env.name());
    let action = Action::Write(bytes);
    apply_action!(action, error_msg, env);
}

fn write_chars(env: &ForeignFunctionEnv, chars_to_write: String) {
    let error_msg = || format!("failed to write in plugin {}", env.plugin_env.name());
    let action = Action::WriteChars(chars_to_write);
    apply_action!(action, error_msg, env);
}

fn toggle_tab(env: &ForeignFunctionEnv) {
    let error_msg = || format!("Failed to toggle tab");
    let action = Action::ToggleTab;
    apply_action!(action, error_msg, env);
}

fn move_pane(env: &ForeignFunctionEnv) {
    let error_msg = || format!("failed to move pane in plugin {}", env.plugin_env.name());
    let action = Action::MovePane(None);
    apply_action!(action, error_msg, env);
}

fn move_pane_with_direction(env: &ForeignFunctionEnv, direction: Direction) {
    let error_msg = || format!("failed to move pane in plugin {}", env.plugin_env.name());
    let action = Action::MovePane(Some(direction));
    apply_action!(action, error_msg, env);
}

fn clear_screen(env: &ForeignFunctionEnv) {
    let error_msg = || format!("failed to clear screen in plugin {}", env.plugin_env.name());
    let action = Action::ClearScreen;
    apply_action!(action, error_msg, env);
}
fn scroll_up(env: &ForeignFunctionEnv) {
    let error_msg = || format!("failed to scroll up in plugin {}", env.plugin_env.name());
    let action = Action::ScrollUp;
    apply_action!(action, error_msg, env);
}

fn scroll_down(env: &ForeignFunctionEnv) {
    let error_msg = || format!("failed to scroll down in plugin {}", env.plugin_env.name());
    let action = Action::ScrollDown;
    apply_action!(action, error_msg, env);
}

fn scroll_to_top(env: &ForeignFunctionEnv) {
    let error_msg = || format!("failed to scroll in plugin {}", env.plugin_env.name());
    let action = Action::ScrollToTop;
    apply_action!(action, error_msg, env);
}

fn scroll_to_bottom(env: &ForeignFunctionEnv) {
    let error_msg = || format!("failed to scroll in plugin {}", env.plugin_env.name());
    let action = Action::ScrollToBottom;
    apply_action!(action, error_msg, env);
}

fn page_scroll_up(env: &ForeignFunctionEnv) {
    let error_msg = || format!("failed to scroll in plugin {}", env.plugin_env.name());
    let action = Action::PageScrollUp;
    apply_action!(action, error_msg, env);
}

fn page_scroll_down(env: &ForeignFunctionEnv) {
    let error_msg = || format!("failed to scroll in plugin {}", env.plugin_env.name());
    let action = Action::PageScrollDown;
    apply_action!(action, error_msg, env);
}

fn toggle_focus_fullscreen(env: &ForeignFunctionEnv) {
    let error_msg = || {
        format!(
            "failed to toggle full screen in plugin {}",
            env.plugin_env.name()
        )
    };
    let action = Action::ToggleFocusFullscreen;
    apply_action!(action, error_msg, env);
}

fn toggle_pane_frames(env: &ForeignFunctionEnv) {
    let error_msg = || {
        format!(
            "failed to toggle full screen in plugin {}",
            env.plugin_env.name()
        )
    };
    let action = Action::TogglePaneFrames;
    apply_action!(action, error_msg, env);
}

fn toggle_pane_embed_or_eject(env: &ForeignFunctionEnv) {
    let error_msg = || {
        format!(
            "failed to toggle pane embed or eject in plugin {}",
            env.plugin_env.name()
        )
    };
    let action = Action::TogglePaneEmbedOrFloating;
    apply_action!(action, error_msg, env);
}

fn undo_rename_pane(env: &ForeignFunctionEnv) {
    let error_msg = || {
        format!(
            "failed to undo rename pane in plugin {}",
            env.plugin_env.name()
        )
    };
    let action = Action::UndoRenamePane;
    apply_action!(action, error_msg, env);
}

fn close_focus(env: &ForeignFunctionEnv) {
    let error_msg = || {
        format!(
            "failed to close focused pane in plugin {}",
            env.plugin_env.name()
        )
    };
    let action = Action::CloseFocus;
    apply_action!(action, error_msg, env);
}

fn toggle_active_tab_sync(env: &ForeignFunctionEnv) {
    let error_msg = || {
        format!(
            "failed to toggle active tab sync in plugin {}",
            env.plugin_env.name()
        )
    };
    let action = Action::ToggleActiveSyncTab;
    apply_action!(action, error_msg, env);
}

fn close_focused_tab(env: &ForeignFunctionEnv) {
    let error_msg = || {
        format!(
            "failed to close active tab in plugin {}",
            env.plugin_env.name()
        )
    };
    let action = Action::CloseTab;
    apply_action!(action, error_msg, env);
}

fn undo_rename_tab(env: &ForeignFunctionEnv) {
    let error_msg = || {
        format!(
            "failed to undo rename tab in plugin {}",
            env.plugin_env.name()
        )
    };
    let action = Action::UndoRenameTab;
    apply_action!(action, error_msg, env);
}

fn quit_zellij(env: &ForeignFunctionEnv) {
    let error_msg = || format!("failed to quit zellij in plugin {}", env.plugin_env.name());
    let action = Action::Quit;
    apply_action!(action, error_msg, env);
}

fn previous_swap_layout(env: &ForeignFunctionEnv) {
    let error_msg = || {
        format!(
            "failed to switch swap layout in plugin {}",
            env.plugin_env.name()
        )
    };
    let action = Action::PreviousSwapLayout;
    apply_action!(action, error_msg, env);
}

fn next_swap_layout(env: &ForeignFunctionEnv) {
    let error_msg = || {
        format!(
            "failed to switch swap layout in plugin {}",
            env.plugin_env.name()
        )
    };
    let action = Action::NextSwapLayout;
    apply_action!(action, error_msg, env);
}

fn go_to_tab_name(env: &ForeignFunctionEnv, tab_name: String) {
    let error_msg = || format!("failed to change tab in plugin {}", env.plugin_env.name());
    let create = false;
    let action = Action::GoToTabName(tab_name, create);
    apply_action!(action, error_msg, env);
}

fn focus_or_create_tab(env: &ForeignFunctionEnv, tab_name: String) {
    let error_msg = || {
        format!(
            "failed to change or create tab in plugin {}",
            env.plugin_env.name()
        )
    };
    let create = true;
    let action = Action::GoToTabName(tab_name, create);
    apply_action!(action, error_msg, env);
}

fn go_to_tab(env: &ForeignFunctionEnv, tab_index: u32) {
    let error_msg = || {
        format!(
            "failed to change tab focus in plugin {}",
            env.plugin_env.name()
        )
    };
    let action = Action::GoToTab(tab_index);
    apply_action!(action, error_msg, env);
}

fn start_or_reload_plugin(env: &ForeignFunctionEnv, url: &str) -> Result<()> {
    let error_msg = || {
        format!(
            "failed to start or reload plugin in plugin {}",
            env.plugin_env.name()
        )
    };
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let url = Url::parse(&url).map_err(|e| anyhow!("Failed to parse url: {}", e))?;
    let run_plugin_location = RunPluginLocation::parse(url.as_str(), Some(cwd))
        .map_err(|e| anyhow!("Failed to parse plugin location: {}", e))?;
    let run_plugin = RunPlugin {
        location: run_plugin_location,
        _allow_exec_host_cmd: false,
    };
    let action = Action::StartOrReloadPlugin(run_plugin);
    apply_action!(action, error_msg, env);
    Ok(())
}

fn close_terminal_pane(env: &ForeignFunctionEnv, terminal_pane_id: u32) {
    let error_msg = || {
        format!(
            "failed to change tab focus in plugin {}",
            env.plugin_env.name()
        )
    };
    let action = Action::CloseTerminalPane(terminal_pane_id);
    apply_action!(action, error_msg, env);
}

fn close_plugin_pane(env: &ForeignFunctionEnv, plugin_pane_id: u32) {
    let error_msg = || {
        format!(
            "failed to change tab focus in plugin {}",
            env.plugin_env.name()
        )
    };
    let action = Action::ClosePluginPane(plugin_pane_id);
    apply_action!(action, error_msg, env);
}

fn focus_terminal_pane(
    env: &ForeignFunctionEnv,
    terminal_pane_id: u32,
    should_float_if_hidden: bool,
) {
    let action = Action::FocusTerminalPaneWithId(terminal_pane_id, should_float_if_hidden);
    let error_msg = || format!("Failed to focus terminal pane");
    apply_action!(action, error_msg, env);
}

fn focus_plugin_pane(env: &ForeignFunctionEnv, plugin_pane_id: u32, should_float_if_hidden: bool) {
    let action = Action::FocusPluginPaneWithId(plugin_pane_id, should_float_if_hidden);
    let error_msg = || format!("Failed to focus plugin pane");
    apply_action!(action, error_msg, env);
}

fn rename_terminal_pane(env: &ForeignFunctionEnv, terminal_pane_id: u32, new_name: &str) {
    let error_msg = || format!("Failed to rename terminal pane");
    let rename_pane_action =
        Action::RenameTerminalPane(terminal_pane_id, new_name.as_bytes().to_vec());
    apply_action!(rename_pane_action, error_msg, env);
}

fn rename_plugin_pane(env: &ForeignFunctionEnv, plugin_pane_id: u32, new_name: &str) {
    let error_msg = || format!("Failed to rename plugin pane");
    let rename_pane_action = Action::RenamePluginPane(plugin_pane_id, new_name.as_bytes().to_vec());
    apply_action!(rename_pane_action, error_msg, env);
}

fn rename_tab(env: &ForeignFunctionEnv, tab_index: u32, new_name: &str) {
    let error_msg = || format!("Failed to rename tab");
    let rename_tab_action = Action::RenameTab(tab_index, new_name.as_bytes().to_vec());
    apply_action!(rename_tab_action, error_msg, env);
}

// Custom panic handler for plugins.
//
// This is called when a panic occurs in a plugin. Since most panics will likely originate in the
// code trying to deserialize an `Event` upon a plugin state update, we read some panic message,
// formatted as string from the plugin.
fn report_panic(env: &ForeignFunctionEnv, msg: &str) {
    log::error!("PANIC IN PLUGIN!\n\r{}", msg);
    handle_plugin_crash(
        env.plugin_env.plugin_id,
        msg.to_owned(),
        env.plugin_env.senders.clone(),
    );
}

// Helper Functions ---------------------------------------------------------------------------------------------------

pub fn wasi_read_string(wasi_env: &WasiEnv) -> Result<String> {
    let err_context = || format!("failed to read string from WASI env '{wasi_env:?}'");

    let mut buf = vec![];
    wasi_env
        .state()
        .fs
        .stdout_mut()
        .map_err(anyError::new)
        .and_then(|stdout| {
            stdout
                .as_mut()
                .ok_or(anyhow!("failed to get mutable reference to stdout"))
        })
        .and_then(|wasi_file| wasi_file.read_to_end(&mut buf).map_err(anyError::new))
        .with_context(err_context)?;
    let buf = String::from_utf8_lossy(&buf);
    // https://stackoverflow.com/questions/66450942/in-rust-is-there-a-way-to-make-literal-newlines-in-r-using-windows-c
    Ok(buf.replace("\n", "\n\r"))
}

pub fn wasi_write_string(wasi_env: &WasiEnv, buf: &str) -> Result<()> {
    wasi_env
        .state()
        .fs
        .stdin_mut()
        .map_err(anyError::new)
        .and_then(|stdin| {
            stdin
                .as_mut()
                .ok_or(anyhow!("failed to get mutable reference to stdin"))
        })
        .and_then(|stdin| writeln!(stdin, "{}\r", buf).map_err(anyError::new))
        .with_context(|| format!("failed to write string to WASI env '{wasi_env:?}'"))
}

pub fn wasi_write_object(wasi_env: &WasiEnv, object: &(impl Serialize + ?Sized)) -> Result<()> {
    serde_json::to_string(&object)
        .map_err(anyError::new)
        .and_then(|string| wasi_write_string(wasi_env, &string))
        .with_context(|| format!("failed to serialize object for WASI env '{wasi_env:?}'"))
}

pub fn wasi_read_bytes(wasi_env: &WasiEnv) -> Result<Vec<u8>> {
    wasi_read_string(wasi_env)
        .and_then(|string| serde_json::from_str(&string).map_err(anyError::new))
        .with_context(|| format!("failed to deserialize object from WASI env '{wasi_env:?}'"))
}
