use crate::domain::{DomainId, WriterWrapper};
use crate::localpane::LocalPane;
use crate::pane::alloc_pane_id;
use crate::tab::{Tab, TabId, SplitDirection, SplitRequest, SplitSize};
use crate::tmux::{TmuxDomain, TmuxDomainState, TmuxRemotePane, TmuxTab};
use crate::tmux_pty::{TmuxChild, TmuxPty};
use crate::{Mux, Pane};
use anyhow::{anyhow, Context};
use parking_lot::{Condvar, Mutex};
use portable_pty::{MasterPty, PtySize};
use fancy_regex::Regex;
use std::collections::HashSet;
use std::fmt::{Debug, Write};
use std::io::Write as _;
use std::sync::Arc;
use termwiz::tmux_cc::*;
use wezterm_term::TerminalSize;

pub(crate) trait TmuxCommand: Send + Debug {
    fn get_command(&self) -> String;
    fn process_result(&self, domain_id: DomainId, result: &Guarded) -> anyhow::Result<()>;
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct PaneItem {
    session_id: TmuxSessionId,
    window_id: TmuxWindowId,
    pane_id: TmuxPaneId,
    _pane_index: u64,
    cursor_x: u64,
    cursor_y: u64,
    pane_width: u64,
    pane_height: u64,
    pane_left: u64,
    pane_top: u64,
    pane_active: bool,
}

#[derive(Debug)]
enum TmuxLayout {
    SplitVertical(Vec<PaneItem>),
    SplitHorizontal(Vec<PaneItem>),
    SinglePane(PaneItem),
}

#[derive(Debug)]
struct WindowItem {
    session_id: TmuxSessionId,
    window_id: TmuxWindowId,
    window_width: u64,
    window_height: u64,
    window_active: bool,
    layout: Vec<TmuxLayout>,
}

impl TmuxDomainState {
    /// check if a PaneItem received from ListAllPanes has been attached
    fn check_pane_attached(&self, target: &PaneItem) -> bool {
        let pane_list = self.gui_tabs.lock();
        let local_tab = match pane_list
            .iter()
            .find(|&x| x.tmux_window_id == target.window_id)
        {
            Some(x) => x,
            None => {
                return false;
            }
        };
        match local_tab.panes.get(&target.pane_id) {
            Some(_) => {
                return true;
            }
            None => {
                return false;
            }
        }
    }

    /// after we create a tab for a remote pane, save its ID into the
    /// TmuxPane-TmuxPane tree, so we can ref it later.
    fn add_attached_pane(&self, target: &PaneItem, tab_id: &TabId) -> anyhow::Result<()> {
        let mut pane_list = self.gui_tabs.lock();
        let local_tab = match pane_list
            .iter_mut()
            .find(|x| x.tmux_window_id == target.window_id)
        {
            Some(x) => x,
            None => {
                pane_list.push(TmuxTab {
                    tab_id: *tab_id,
                    tmux_window_id: target.window_id,
                    panes: HashSet::new(),
                });
                pane_list.last_mut().unwrap()
            }
        };
        match local_tab.panes.get(&target.pane_id) {
            Some(_) => {
                anyhow::bail!("Tmux pane already attached");
            }
            None => {
                local_tab.panes.insert(target.pane_id);
                return Ok(());
            }
        }
    }

    fn create_pane(&self, window: &WindowItem, pane: &PaneItem) -> anyhow::Result<Arc<dyn Pane>> {
        let local_pane_id = alloc_pane_id();
        let active_lock = Arc::new((Mutex::new(false), Condvar::new()));
        let (output_read, output_write) = filedescriptor::socketpair()?;
        let ref_pane = Arc::new(Mutex::new(TmuxRemotePane {
            local_pane_id,
            output_write,
            active_lock: active_lock.clone(),
            session_id: window.session_id,
            window_id: window.window_id,
            pane_id: pane.pane_id,
            cursor_x: pane.cursor_x,
            cursor_y: pane.cursor_y,
            pane_width: pane.pane_width,
            pane_height: pane.pane_height,
            pane_left: pane.pane_left,
            pane_top: pane.pane_top,
        }));

        {
            let mut pane_map = self.remote_panes.lock();
            pane_map.insert(pane.pane_id, ref_pane.clone());
        }

        let pane_pty = TmuxPty {
            domain_id: self.domain_id,
            reader: output_read,
            cmd_queue: self.cmd_queue.clone(),
            master_pane: ref_pane,
        };

        let writer = WriterWrapper::new(pane_pty.take_writer()?);

        let size = TerminalSize {
            rows: pane.pane_height as usize,
            cols: pane.pane_width as usize,
            pixel_width: 0,
            pixel_height: 0,
            dpi: 0,
        };

        let child = TmuxChild {
            active_lock: active_lock.clone(),
        };

        let terminal = wezterm_term::Terminal::new(
            size,
            std::sync::Arc::new(config::TermConfig::new()),
            "WezTerm",
            config::wezterm_version(),
            Box::new(writer.clone()),
        );

        Ok(Arc::new(LocalPane::new(
                local_pane_id,
                terminal,
                Box::new(child),
                Box::new(pane_pty),
                Box::new(writer),
                self.domain_id,
                "tmux pane".to_string(),
        )))
    }

    fn sync_pane_state(&self, panes: &[PaneItem]) -> anyhow::Result<()> {
        let current_session = self.tmux_session.lock().unwrap_or(0);
        let mux = Mux::get();

        for pane in panes.iter() {
            if pane.session_id != current_session || !self.check_pane_attached(&pane) {
                continue;
            }

            //Set active pane
            if pane.pane_active {
                let pane_list = self.gui_tabs.lock();
                let local_tab_id = match pane_list
                    .iter()
                    .find(|&x| x.tmux_window_id == pane.window_id)
                    {
                        Some(x) => x.tab_id,
                        None => 0,
                    };
                let tab = mux.get_tab(local_tab_id).unwrap();
                let pane_map = self.remote_panes.lock();
                let local_pane_id = pane_map.get(&pane.pane_id).unwrap().lock().local_pane_id;
                let local_pane = mux.get_pane(local_pane_id).unwrap();
                tab.set_active_pane(&local_pane);
            }

            self.cmd_queue
                .lock()
                .push_back(Box::new(CapturePane(pane.pane_id)));
            TmuxDomainState::schedule_send_next_command(self.domain_id);

            log::info!("new pane synced, id: {}", pane.pane_id);
        }
        Ok(())
    }

    fn sync_window_state(&self, windows: &[WindowItem]) -> anyhow::Result<()> {
        let current_session = self.tmux_session.lock().unwrap_or(0);
        let mux = Mux::get();

        self.create_gui_window();
        let mut gui_window = self.gui_window.lock();
        let gui_window_id = match gui_window.as_mut() {
            Some(x) => x,
            None => {
                anyhow::bail!("No tmux gui created");
            }
        };

        let mut session_id = 0;
        let mut active_tab_id = None;
        for window in windows.iter() {
            if window.session_id != current_session {
                continue;
            }

            let size = TerminalSize {
                rows: window.window_height as usize,
                cols: window.window_width as usize,
                pixel_width: 0,
                pixel_height: 0,
                dpi: 0,
            };

            let tab = Arc::new(Tab::new(&size));
            tab.set_title(&format!("Tmux window: {}", window.window_id));
            mux.add_tab_no_panes(&tab);

            if window.window_active {
                active_tab_id = Some(tab.tab_id());
            }

            let mut split_stack;
            let mut split_direction;

            let mut split_pane_index = 1;
            for l in &window.layout {
                match l {
                    TmuxLayout::SinglePane(p) => {
                        let local_pane = self.create_pane(window, &p).unwrap();
                        tab.assign_pane(&local_pane);
                        self.add_attached_pane(&p, &tab.tab_id())?;
                        let _ = mux.add_pane(&local_pane);
                        break;
                    }

                    TmuxLayout::SplitHorizontal(x) => {
                        split_direction = SplitDirection::Horizontal;
                        split_stack = x;
                    }

                    TmuxLayout::SplitVertical(x) => {
                        split_direction = SplitDirection::Vertical;
                        split_stack = x;
                   }
                }

                for p in split_stack {
                    let local_pane;
                    if !self.check_pane_attached(&p) {
                        local_pane = self.create_pane(window, &p).unwrap();
                        self.add_attached_pane(&p, &tab.tab_id())?;
                        let _ = mux.add_pane(&local_pane);
                        if let None = tab.get_active_pane() {
                            tab.assign_pane(&local_pane);
                            split_pane_index = tab.get_active_idx();
                            continue;
                        }

                        split_pane_index = tab.split_and_insert(
                            split_pane_index,
                            SplitRequest {
                                direction: split_direction,
                                target_is_second: false,
                                top_level: false,
                                size: SplitSize::Cells(
                                    if split_direction == SplitDirection::Horizontal {
                                        p.pane_width as usize
                                    } else {
                                        p.pane_height as usize
                                    })
                            },
                            local_pane.clone()
                        )? + 1;

                    } else {
                        let pane_map = self.remote_panes.lock();
                        let local_pane_id = pane_map.get(&p.pane_id).unwrap().lock().local_pane_id;
                        split_pane_index = match tab
                            .iter_panes_ignoring_zoom()
                            .iter()
                            .find(|x| x.pane.pane_id() == local_pane_id)
                        {
                            Some(x) => x.index,
                            None => anyhow::bail!("invalid pane id {}", local_pane_id),
                        };
                        continue;
                    }
                }
            }

            mux.add_tab_to_window(&tab, **gui_window_id)?;
            gui_window_id.notify();

            session_id = window.session_id;
        }

        if let Some(mut window) = mux.get_window_mut(**gui_window_id) {
            window.set_title(&format!("Tmux Session: {session_id}"));

            if let Some(idx) = active_tab_id {
                let tab_idx = window
                    .idx_by_id(idx)
                    .ok_or_else(|| anyhow::anyhow!("tab {idx} not in {}", **gui_window_id))?;
                window.save_and_then_set_active(tab_idx);
            }
        }

        self.cmd_queue.lock().push_back(Box::new(ListAllPanes));
        TmuxDomainState::schedule_send_next_command(self.domain_id);

        Ok(())
    }
}

#[derive(Debug)]
pub(crate) struct ListAllPanes;
impl TmuxCommand for ListAllPanes {
    fn get_command(&self) -> String {
        "list-panes -aF '#{session_id} #{window_id} #{pane_id} \
            #{pane_index} #{cursor_x} #{cursor_y} #{pane_width} #{pane_height} \
            #{pane_left} #{pane_top} #{pane_active}'\n"
            .to_owned()
    }

    fn process_result(&self, domain_id: DomainId, result: &Guarded) -> anyhow::Result<()> {
        let mut items = vec![];

        for line in result.output.split('\n') {
            if line.is_empty() {
                continue;
            }
            let mut fields = line.split(' ');
            let session_id = fields.next().ok_or_else(|| anyhow!("missing session_id"))?;
            let window_id = fields.next().ok_or_else(|| anyhow!("missing window_id"))?;
            let pane_id = fields.next().ok_or_else(|| anyhow!("missing pane_id"))?;
            let _pane_index = fields
                .next()
                .ok_or_else(|| anyhow!("missing pane_index"))?
                .parse()?;
            let cursor_x = fields
                .next()
                .ok_or_else(|| anyhow!("missing cursor_x"))?
                .parse()?;
            let cursor_y = fields
                .next()
                .ok_or_else(|| anyhow!("missing cursor_y"))?
                .parse()?;
            let pane_width = fields
                .next()
                .ok_or_else(|| anyhow!("missing pane_width"))?
                .parse()?;
            let pane_height = fields
                .next()
                .ok_or_else(|| anyhow!("missing pane_height"))?
                .parse()?;
            let pane_left = fields
                .next()
                .ok_or_else(|| anyhow!("missing pane_left"))?
                .parse()?;
            let pane_top = fields
                .next()
                .ok_or_else(|| anyhow!("missing pane_top"))?
                .parse()?;
            let pane_active = fields
                .next()
                .ok_or_else(|| anyhow!("missing pane_active"))?
                .parse::<usize>()?;

            // These ids all have various sigils such as `$`, `%`, `@`,
            // so skip those prior to parsing them
            let session_id = session_id[1..].parse()?;
            let window_id = window_id[1..].parse()?;
            let pane_id = pane_id[1..].parse()?;
            let pane_active = if pane_active == 1 {true} else {false};

            items.push(PaneItem {
                session_id,
                window_id,
                pane_id,
                _pane_index,
                cursor_x,
                cursor_y,
                pane_width,
                pane_height,
                pane_left,
                pane_top,
                pane_active,
            });
        }

        log::info!("panes in domain_id {}: {:?}", domain_id, items);
        let mux = Mux::get();
        if let Some(domain) = mux.get_domain(domain_id) {
            if let Some(tmux_domain) = domain.downcast_ref::<TmuxDomain>() {
                return tmux_domain.inner.sync_pane_state(&items);
            }
        }
        anyhow::bail!("Tmux domain lost");
    }
}

fn parse_pane(layout: &str) -> anyhow::Result<PaneItem> {
    let re_pane = Regex::new(r"^,?(\d+)x(\d+),(\d+),(\d+)(,(\d+))?").unwrap();

    if let Some(caps) = re_pane.captures(layout).unwrap() {

        let pane_width = caps.get(1).unwrap().as_str().parse().expect("Wrong pane width");
        let pane_height = caps.get(2).unwrap().as_str().parse().expect("Wrong pane height");
        let pane_left = caps.get(3).unwrap().as_str().parse().expect("Wrong pane left");
        let pane_top = caps.get(4).unwrap().as_str().parse().expect("Wrong pane top");
        let pane_id = match caps.get(6) {
            Some(x) => x.as_str().parse().expect(""),
            None => 0
        };

        return Ok(PaneItem {
            _pane_index: 0, // Don't care
            session_id: 0,  // Will fill it later
            window_id: 0,
            pane_id,
            pane_width,
            pane_height,
            pane_left,
            pane_top,
            cursor_x: 0, // the layout doesn't include this information, will set by list-panes
            cursor_y: 0,
            pane_active: false, // same as above
        });
    }

    anyhow::bail!("Wrong pane layout format");
}

fn parse_layout(mut layout: &str, result: &mut Vec<TmuxLayout>,
                mut stack: Option<Vec<PaneItem>>) -> anyhow::Result<usize> {
    let re_pane        = Regex::new(r"^,?(\d+x\d+,\d+,\d+,\d+)").unwrap();
    let re_split_push  = Regex::new(r"^,?(\d+x\d+,\d+,\d+)[\{|\[]").unwrap();
    let re_split_h_pop = Regex::new(r"^\}").unwrap();
    let re_split_v_pop = Regex::new(r"^\]").unwrap();

    let mut parse_len = 0;

    log::debug!("Parsing tmux layout: '{}'", layout);
    while layout.len() > 0 {
        if let Some(caps) = re_split_push.captures(layout).unwrap() {
            log::debug!("Tmux layout split");
            let mut new_stack: Vec<PaneItem> = Vec::new();
            let len = caps.get(0).unwrap().as_str().len();
            let pane = parse_pane(caps.get(1).unwrap().as_str()).unwrap();
            new_stack.push(pane);
            parse_len += len;

            layout = layout.get(len..).unwrap();
            let len = parse_layout(layout, result, Some(new_stack))?;

            if let Some(ref mut x) = stack {
                // Copy the first item of inner layout to outer layout, we creat panes from outer to inner.
                let pane = match &result[0] {
                    TmuxLayout::SplitHorizontal(x) => {
                        x[0].clone()
                    }
                    TmuxLayout::SplitVertical(x) => {
                        x[0].clone()
                    }
                    TmuxLayout::SinglePane(_x) => {
                        anyhow::bail!("The tmux layout is not right")
                    }
                };
                x.push(pane);
            }

            layout = layout.get(len..).unwrap();
            parse_len += len;
        } else if let Some(caps) = re_pane.captures(layout).unwrap() {
            log::debug!("Tmux layout pane");
            let len = caps.get(0).unwrap().as_str().len();
            let pane = parse_pane(caps.get(1).unwrap().as_str()).unwrap();
            match stack {
                Some(ref mut x) => {
                    x.push(pane);
                    parse_len += len;
                    layout = layout.get(len..).unwrap();
                }
                None => {
                    result.push(TmuxLayout::SinglePane(pane));
                    return Ok(parse_len + len);
                }
            }
        } else if let Some(_caps) = re_split_h_pop.captures(layout).unwrap() {
            log::debug!("Tmux layout split horizontal pop");
            if let Some(ref mut x) = stack {
                let pane = x.pop().unwrap();
                x[0].pane_id = pane.pane_id;
                result.insert(0, TmuxLayout::SplitHorizontal(stack.unwrap()));
                return Ok(parse_len + 1);
            }
        } else if let Some(_caps) = re_split_v_pop.captures(layout).unwrap() {
            log::debug!("Tmux layout split vertical pop");
            if let Some(mut x) = stack {
                let pane = x.pop().unwrap();
                x[0].pane_id = pane.pane_id;
                result.insert(0, TmuxLayout::SplitVertical(x));
                return Ok(parse_len + 1);
            }
        }
    }

    Ok(0)
}

#[derive(Debug)]
pub(crate) struct ListAllWindows;
impl TmuxCommand for ListAllWindows {
    fn get_command(&self) -> String {
        "list-windows -aF '#{session_id} #{window_id} #{window_width} #{window_height} #{window_active} #{window_layout}'\n".to_owned()
    }

    fn process_result(&self, domain_id: DomainId, result: &Guarded) -> anyhow::Result<()> {
        let mut items = vec![];

        for line in result.output.split('\n') {
            if line.is_empty() {
                continue;
            }
            let mut fields = line.split(' ');
            let session_id = fields.next().ok_or_else(|| anyhow!("missing session_id"))?;
            let window_id = fields.next().ok_or_else(|| anyhow!("missing window_id"))?;
            let window_width = fields
                .next()
                .ok_or_else(|| anyhow!("missing window_width"))?
                .parse()?;
            let window_height = fields
                .next()
                .ok_or_else(|| anyhow!("missing window_height"))?
                .parse()?;
            let window_active = fields
                .next()
                .ok_or_else(|| anyhow!("missing window_active"))?
                .parse::<usize>()?;

            let window_layout = fields.next().ok_or_else(|| anyhow!("missing window_layout"))?;

            // These ids all have various sigils such as `$`, `%`, `@`,
            // so skip those prior to parsing them
            let session_id = session_id[1..].parse()?;
            let window_id = window_id[1..].parse()?;

            let window_layout = window_layout.get(5..).unwrap();

            let mut layout = Vec::<TmuxLayout>::new();

            let _ = parse_layout(window_layout, &mut layout, None)?;
            // Fill in the session_id and window_id
            for l in &mut layout {
                match l {
                    TmuxLayout::SinglePane(ref mut p) => {
                        p.session_id = session_id;
                        p.window_id = window_id;
                    }
                    TmuxLayout::SplitHorizontal(ref mut v) => {
                        for p in v {
                            p.session_id = session_id;
                            p.window_id = window_id;
                        }
                    }
                    TmuxLayout::SplitVertical(ref mut v) => {
                        for p in v {
                            p.session_id = session_id;
                            p.window_id = window_id;
                        }
                    }
                }
            }

            let window_active = if window_active == 1 {true} else {false};

            items.push(WindowItem {
                session_id,
                window_id,
                window_width,
                window_height,
                window_active,
                layout,
            });
        }

        log::info!("layout in domain_id {}: {:#?}", domain_id, items);
        let mux = Mux::get();
        if let Some(domain) = mux.get_domain(domain_id) {
            if let Some(tmux_domain) = domain.downcast_ref::<TmuxDomain>() {
                return tmux_domain.inner.sync_window_state(&items);
            }
        }
        anyhow::bail!("Tmux domain lost");
    }
}

#[derive(Debug)]
pub(crate) struct Resize {
    pub pane_id: TmuxPaneId,
    pub size: PtySize,
}

impl TmuxCommand for Resize {
    fn get_command(&self) -> String {
        format!("resize-pane -x {} -y {} -t %{}\n", self.size.cols, self.size.rows, self.pane_id)
    }

    fn process_result(&self, domain_id: DomainId, result: &Guarded) -> anyhow::Result<()> {
        if result.error {
            log::error!(
                "Error resizing: domain_id={} result={:?}",
                domain_id,
                result
            );
        }
        Ok(())
    }
}

#[derive(Debug)]
pub(crate) struct CapturePane(TmuxPaneId);
impl TmuxCommand for CapturePane {
    fn get_command(&self) -> String {
        format!("capture-pane -p -t %{} -e -C\n", self.0)
    }

    fn process_result(&self, domain_id: DomainId, result: &Guarded) -> anyhow::Result<()> {
        let mux = Mux::get();
        let domain = match mux.get_domain(domain_id) {
            Some(d) => d,
            None => anyhow::bail!("Tmux domain lost"),
        };
        let tmux_domain = match domain.downcast_ref::<TmuxDomain>() {
            Some(t) => t,
            None => anyhow::bail!("Tmux domain lost"),
        };

        let unescaped = termwiz::tmux_cc::unvis(&result.output.trim_end_matches('\n')).context("unescape pane content")?;
        // capturep contents returned from guarded lines which always contain a tailing '\n'
        let unescaped = &unescaped[0..unescaped.len()].replace("\n", "\r\n");

        let pane_map = tmux_domain.inner.remote_panes.lock();
        if let Some(pane) = pane_map.get(&self.0) {
            let mut pane = pane.lock();
            pane.output_write
                .write_all(unescaped.as_bytes())
                .context("writing capture pane result to output")?;
        }

        Ok(())
    }
}

#[derive(Debug)]
pub(crate) struct SendKeys {
    pub keys: Vec<u8>,
    pub pane: TmuxPaneId,
}
impl TmuxCommand for SendKeys {
    fn get_command(&self) -> String {
        let mut s = String::new();
        for &byte in self.keys.iter() {
            write!(&mut s, "0x{:X} ", byte).expect("unable to write key");
        }
        format!("send-keys -t %{} {}\r", self.pane, s)
    }

    fn process_result(&self, _domain_id: DomainId, _result: &Guarded) -> anyhow::Result<()> {
        Ok(())
    }
}
