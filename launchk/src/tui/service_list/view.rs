use std::cell::RefCell;
use std::cmp::Ordering;
use std::collections::HashSet;
use std::rc::Rc;
use std::sync::mpsc::Sender;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use cursive::direction::Direction;
use cursive::view::ViewWrapper;
use cursive::{Cursive, View, XY};

use tokio::runtime::Handle;
use tokio::time::interval;
use xpc_sys::objects::unix_fifo::UnixFifo;
use xpc_sys::enums::{DomainType, SessionType};

use crate::launchd::job_type_filter::JobTypeFilter;
use crate::launchd::plist::{edit_and_replace, LABEL_TO_ENTRY_CONFIG};
use crate::launchd::query::procinfo;
use crate::launchd::query::{disable, enable, list_all, load, unload};
use crate::launchd::{
    entry_status::get_entry_status, entry_status::LaunchdEntryStatus, plist::LaunchdPlist,
};
use crate::tui::omnibox::command::OmniboxCommand;

use crate::tui::omnibox::state::OmniboxState;
use crate::tui::omnibox::subscribed_view::{OmniboxResult, OmniboxSubscriber};
use crate::tui::omnibox::view::{OmniboxError, OmniboxEvent, OmniboxMode};
use crate::tui::pager::show_pager;
use crate::tui::root::CbSinkMessage;
use crate::tui::service_list::list_item::ServiceListItem;
use crate::tui::table::table_list_view::TableListView;

/// Polls XPC for job list
async fn poll_running_jobs(svcs: Arc<RwLock<HashSet<String>>>, cb_sink: Sender<CbSinkMessage>) {
    let mut interval = interval(Duration::from_secs(1));

    loop {
        interval.tick().await;
        let write = svcs.try_write();

        if write.is_err() {
            continue;
        }

        let mut write = write.unwrap();
        *write = list_all();

        cb_sink.send(Box::new(Cursive::noop)).expect("Must send");
    }
}

pub struct ServiceListView {
    cb_sink: Sender<CbSinkMessage>,
    running_jobs: Arc<RwLock<HashSet<String>>>,
    table_list_view: TableListView<ServiceListItem>,
    label_filter: RefCell<String>,
    job_type_filter: RefCell<JobTypeFilter>,
}

impl ServiceListView {
    pub fn new(runtime_handle: &Handle, cb_sink: Sender<CbSinkMessage>) -> Self {
        let arc_svc = Arc::new(RwLock::new(HashSet::new()));
        runtime_handle.spawn(poll_running_jobs(arc_svc.clone(), cb_sink.clone()));

        Self {
            cb_sink,
            running_jobs: arc_svc.clone(),
            label_filter: RefCell::new("".into()),
            job_type_filter: RefCell::new(JobTypeFilter::launchk_default()),
            table_list_view: TableListView::new(vec![
                ("Name", None),
                ("Session", Some(12)),
                ("Job Type", Some(14)),
                ("PID", Some(6)),
                ("Loaded", Some(6)),
            ]),
        }
    }

    fn present_services(&self) -> Option<Vec<ServiceListItem>> {
        let plists = LABEL_TO_ENTRY_CONFIG.read().ok()?;
        let running = self.running_jobs.read().ok()?;

        let name_filter = self.label_filter.borrow();
        let job_type_filter = self.job_type_filter.borrow();

        let running_no_plist = running.iter().filter(|r| !plists.contains_key(*r));

        let mut items: Vec<ServiceListItem> = plists
            .keys()
            .into_iter()
            .chain(running_no_plist)
            .filter_map(|label| {
                if !name_filter.is_empty()
                    && !label
                        .to_ascii_lowercase()
                        .contains(name_filter.to_ascii_lowercase().as_str())
                {
                    return None;
                }

                let status = get_entry_status(label);
                let is_loaded = running.contains(label);

                let entry_job_type_filter = status
                    .plist
                    .as_ref()
                    .map(|ec| ec.job_type_filter(is_loaded))
                    .unwrap_or(if is_loaded {
                        JobTypeFilter::LOADED
                    } else {
                        JobTypeFilter::default()
                    });

                if !job_type_filter.is_empty() && !entry_job_type_filter.contains(*job_type_filter)
                {
                    return None;
                }

                Some(ServiceListItem {
                    status,
                    name: label.clone(),
                    job_type_filter: entry_job_type_filter,
                })
            })
            .collect();

        items.sort_by(|a, b| {
            let loaded_a = a.job_type_filter.intersects(JobTypeFilter::LOADED);
            let loaded_b = b.job_type_filter.intersects(JobTypeFilter::LOADED);
            let name_cmp = a.name.cmp(&b.name);

            if !loaded_a && loaded_b {
                Ordering::Less
            } else if loaded_a && !loaded_b {
                Ordering::Greater
            } else {
                name_cmp
            }
        });

        Some(items)
    }

    fn handle_state_update(&mut self, state: OmniboxState) -> OmniboxResult {
        let OmniboxState {
            mode,
            label_filter,
            job_type_filter,
            ..
        } = state;

        match mode {
            OmniboxMode::LabelFilter => {
                self.label_filter.replace(label_filter);
            }
            OmniboxMode::JobTypeFilter => {
                self.job_type_filter.replace(job_type_filter);
            }
            OmniboxMode::Idle => {
                self.label_filter.replace(label_filter);
                self.job_type_filter.replace(job_type_filter);
            }
            _ => {}
        };

        Ok(None)
    }

    fn get_active_list_item(&self) -> Result<Rc<ServiceListItem>, OmniboxError> {
        self.table_list_view
            .get_highlighted_row()
            .ok_or_else(|| OmniboxError::CommandError("Cannot get highlighted row".to_string()))
    }

    fn with_active_item_plist(&self) -> Result<(ServiceListItem, LaunchdPlist), OmniboxError> {
        let item = &*self.get_active_list_item()?;
        let plist = item
            .status
            .plist
            .as_ref()
            .ok_or_else(|| OmniboxError::CommandError("Cannot find plist".to_string()))?;

        Ok((item.clone(), plist.clone()))
    }

    fn handle_command(&self, cmd: OmniboxCommand) -> OmniboxResult {
        match cmd {
            OmniboxCommand::Reload => {
                let (ServiceListItem { name, status, .. }, ..) = self.with_active_item_plist()?;
                let LaunchdEntryStatus {
                    limit_load_to_session_type,
                    domain,
                    ..
                } = status;

                match (limit_load_to_session_type, domain) {
                    (_, DomainType::Unknown) | (SessionType::Unknown, _) => Ok(Some(
                        OmniboxCommand::DomainSessionPrompt(name.clone(), false, |dt, st| {
                            vec![
                                OmniboxCommand::Unload(dt.clone(), None),
                                OmniboxCommand::Load(st.expect("Must provide"), dt, None),
                            ]
                        }),
                    )),
                    (st, dt) => Ok(Some(OmniboxCommand::Chain(vec![
                        OmniboxCommand::Unload(dt.clone(), None),
                        OmniboxCommand::Load(st, dt, None),
                    ]))),
                }
            }
            OmniboxCommand::LoadRequest => {
                let (ServiceListItem { name, .. }, ..) = self.with_active_item_plist()?;
                Ok(Some(OmniboxCommand::DomainSessionPrompt(
                    name.clone(),
                    false,
                    |dt, st| {
                        vec![OmniboxCommand::Load(
                            st.expect("Must be provided"),
                            dt,
                            None,
                        )]
                    },
                )))
            }
            OmniboxCommand::UnloadRequest => {
                let (ServiceListItem { name, status, .. }, ..) = self.with_active_item_plist()?;
                let LaunchdEntryStatus { domain, .. } = status;

                match domain {
                    DomainType::Unknown => Ok(Some(OmniboxCommand::DomainSessionPrompt(
                        name.clone(),
                        true,
                        |dt, _| vec![OmniboxCommand::Unload(dt, None)],
                    ))),
                    _ => Ok(Some(OmniboxCommand::Unload(domain, None))),
                }
            }
            OmniboxCommand::EnableRequest => {
                let (ServiceListItem { name, .. }, ..) = self.with_active_item_plist()?;
                Ok(Some(OmniboxCommand::DomainSessionPrompt(
                    name.clone(),
                    true,
                    |dt, _| vec![OmniboxCommand::Enable(dt)],
                )))
            }
            OmniboxCommand::DisableRequest => {
                let (ServiceListItem { name, status, .. }, ..) = self.with_active_item_plist()?;
                let LaunchdEntryStatus { domain, .. } = status;

                match domain {
                    DomainType::Unknown => Ok(Some(OmniboxCommand::DomainSessionPrompt(
                        name.clone(),
                        true,
                        |dt, _| vec![OmniboxCommand::Disable(dt)],
                    ))),
                    _ => Ok(Some(OmniboxCommand::Chain(vec![OmniboxCommand::Disable(
                        domain,
                    )]))),
                }
            }
            OmniboxCommand::Edit => {
                let (ServiceListItem { name, .. }, plist) = self.with_active_item_plist()?;
                edit_and_replace(&plist).map_err(OmniboxError::CommandError)?;

                // Clear term
                self.cb_sink
                    .send(Box::new(Cursive::clear))
                    .expect("Must clear");

                Ok(Some(OmniboxCommand::Confirm(
                    format!("Reload {}?", name),
                    vec![OmniboxCommand::Reload],
                )))
            }
            OmniboxCommand::Load(st, dt, _handle) => {
                let (ServiceListItem { name, .. }, plist) = self.with_active_item_plist()?;
                load(name, plist.plist_path, Some(dt), Some(st), None)
                    .map(|_| None)
                    .map_err(|e| OmniboxError::CommandError(e.to_string()))
            }
            OmniboxCommand::Unload(dt, _handle) => {
                let (ServiceListItem { name, status, .. }, plist) =
                    self.with_active_item_plist()?;
                let LaunchdEntryStatus {
                    limit_load_to_session_type,
                    ..
                } = status;

                unload(
                    name,
                    plist.plist_path,
                    Some(dt),
                    Some(limit_load_to_session_type),
                    None,
                )
                .map(|_| None)
                .map_err(|e| OmniboxError::CommandError(e.to_string()))
            }
            OmniboxCommand::Enable(dt) => {
                let (ServiceListItem { name, .. }, _) = self.with_active_item_plist()?;
                enable(name, dt)
                    .map(|_| None)
                    .map_err(|e| OmniboxError::CommandError(e.to_string()))
            }
            OmniboxCommand::Disable(dt) => {
                let (ServiceListItem { name, .. }, _) = self.with_active_item_plist()?;
                disable(name, dt)
                    .map(|_| None)
                    .map_err(|e| OmniboxError::CommandError(e.to_string()))
            }
            OmniboxCommand::ProcInfo => {
                let (ServiceListItem { name, status, .. }, _) = self.with_active_item_plist()?;

                if status.pid == 0 {
                    return Err(OmniboxError::CommandError(format!("No PID for {}", name)));
                }

                let fifo =
                    Arc::new(UnixFifo::new(0o777).map_err(|e| OmniboxError::CommandError(e))?);

                let fifo_clone = fifo.clone();

                // Spawn pipe reader
                let fd_read_thread = std::thread::spawn(move || fifo_clone.block_and_read_bytes());

                fifo.with_writer(|fd_write| procinfo(status.pid, fd_write))
                    .map_err(|e| OmniboxError::CommandError(e))?
                    .map_err(|e| OmniboxError::CommandError(e.to_string()))?;

                // Join reader thread (and close fd)
                let procinfo_data = fd_read_thread
                    .join()
                    .expect("Must join read thread")
                    .map_err(|e| OmniboxError::CommandError(e))?;

                show_pager(&self.cb_sink, &procinfo_data)
                    .map_err(|e| OmniboxError::CommandError(e))?;

                Ok(None)
            }
            _ => Ok(None),
        }
    }
}

impl ViewWrapper for ServiceListView {
    wrap_impl!(self.table_list_view: TableListView<ServiceListItem>);

    fn wrap_layout(&mut self, size: XY<usize>) {
        self.table_list_view.layout(size);

        if let Some(sorted) = self.present_services() {
            self.with_view_mut(|v| v.replace_and_preserve_selection(sorted));
        }
    }

    fn wrap_take_focus(&mut self, _: Direction) -> bool {
        true
    }
}

impl OmniboxSubscriber for ServiceListView {
    fn on_omnibox(&mut self, event: OmniboxEvent) -> OmniboxResult {
        match event {
            OmniboxEvent::StateUpdate(state) => self.handle_state_update(state),
            OmniboxEvent::Command(cmd) => self.handle_command(cmd),
        }
    }
}
