use crate::{
    ActivityItem, AppItem, AppRow, Artwork, MainWindow,
    errors::AppError,
    icons,
    metadata::{self, CATEGORIES},
    operations::{Manager, QUEUE_LIMIT, QueueBlock, Status},
    packages::Package,
    platform,
    scoop::{self, Scoop},
    scoop_catalog,
    storage::{Settings, Storage},
    winget::{self, Request, Winget},
};
use slint::{ComponentHandle, ModelRc, SharedString, VecModel};
use std::{
    cell::RefCell,
    collections::{BTreeMap, HashMap, HashSet, VecDeque},
    rc::Rc,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
        mpsc,
    },
    time::Duration,
};

thread_local! {
    static STATE:RefCell<Option<Rc<RefCell<State>>>> = const { RefCell::new(None) };
    static UI_WEAK:RefCell<Option<slint::Weak<MainWindow>>> = const { RefCell::new(None) };
}

pub(crate) fn with_window<T>(f: impl FnOnce(&MainWindow) -> T) -> Option<T> {
    let weak = UI_WEAK.with(|slot| slot.borrow().clone())?;
    weak.upgrade().as_ref().map(f)
}

#[derive(Clone, Copy)]
enum BatchAction {
    Install,
    InstallScoop,
    Uninstall,
    Upgrade,
}

impl BatchAction {
    fn verb(self) -> &'static str {
        match self {
            Self::Install | Self::InstallScoop => "install",
            Self::Uninstall => "uninstall",
            Self::Upgrade => "upgrade",
        }
    }
}

#[derive(Clone)]
enum ReadKind {
    Shutdown,
    Detect,
    Search(String),
    DetectScoop,
    Category(String),
    Inventory(bool),
    Details(Package),
    Batch(BatchAction, Vec<Package>),
    Sources,
}
struct Job {
    generation: u64,
    kind: ReadKind,
    source_consent: bool,
}
struct CatalogJob {
    generation: u64,
    query: String,
    force: bool,
    persist: bool,
    additional: bool,
}
enum Response {
    Detection(String, String),
    BackendError(String),
    Packages(Vec<Package>, usize, Option<String>),
    WinGetSearch(Result<winget::Table, AppError>),
    ScoopSearch(scoop_catalog::Search),
    ScoopDetection(bool),
    Inventory(Vec<Package>, usize, bool, Option<String>),
    Details(Package, String, bool),
    Batch(BatchAction, Vec<Package>, Vec<String>),
    Sources(String),
    Operations(bool),
    Error(String),
}

fn post(ui: &slint::Weak<MainWindow>, generation: Option<u64>, event: Response) {
    let _ = ui.upgrade_in_event_loop(move |ui| {
        STATE.with(|slot| {
            let state = slot.borrow().clone();
            if let Some(state) = state {
                receive(&ui, &state, generation, event);
            }
        });
    });
}

fn read_worker(
    winget: Winget,
    mut scoop: Option<Scoop>,
    receiver: mpsc::Receiver<Job>,
    generation: Arc<AtomicU64>,
    ui: slint::Weak<MainWindow>,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        loop {
            if generation.load(Ordering::Relaxed) == u64::MAX {
                return;
            }
            let job = match receiver.recv() {
                Ok(job) => job,
                Err(_) => return,
            };
            let diagnostic = matches!(&job.kind, ReadKind::Detect);
            let obsolete = || {
                let current = generation.load(Ordering::Relaxed);
                current == u64::MAX || (!diagnostic && current != job.generation)
            };
            if obsolete() {
                continue;
            }
            let result = (|| -> Result<Response, AppError> {
                match job.kind {
                    ReadKind::Shutdown => Err(AppError("SlimStore is closing".into())),
                    ReadKind::Detect => {
                        let version = winget
                            .read(&Request::Version, false, obsolete)?
                            .checked()?
                            .text;
                        let sources = winget
                            .read(&Request::Sources, false, obsolete)
                            .and_then(|o| o.checked())
                            .map(|o| winget::source_summary(&o.text))
                            .unwrap_or_else(|e| e.to_string());
                        let com = platform::probe_com();
                        Ok(Response::Detection(
                            format!(
                                "System WinGet {version}\n{com}\n\nSearch, details, installation, and inventory use the existing WinGet CLI. No bundled package manager or runtime is extracted."
                            ),
                            sources,
                        ))
                    }
                    ReadKind::Search(query) => Ok(Response::WinGetSearch(
                        winget
                            .read(&Request::Search(query), job.source_consent, obsolete)
                            .and_then(|output| winget::parse_table(&output, false)),
                    )),
                    ReadKind::DetectScoop => {
                        scoop = Scoop::detect();
                        Ok(Response::ScoopDetection(scoop.is_some()))
                    }
                    ReadKind::Category(category) => {
                        let tag = metadata::category_tag(&category).ok_or_else(|| {
                            AppError("Choose a category with a supported WinGet tag.".into())
                        })?;
                        let result = winget::parse_table(
                            &winget.read(
                                &Request::Tag(tag.into()),
                                job.source_consent,
                                obsolete,
                            )?,
                            false,
                        )?;
                        Ok(Response::Packages(
                            result.packages,
                            result.skipped,
                            Some(category),
                        ))
                    }
                    ReadKind::Inventory(updates) => {
                        let result = winget
                            .read(
                                &if updates {
                                    Request::Updates
                                } else {
                                    Request::Installed
                                },
                                job.source_consent,
                                obsolete,
                            )
                            .and_then(|output| winget::parse_table(&output, true));
                        let mut scooped = scoop.as_ref().map(Scoop::inventory).unwrap_or_default();
                        match result {
                            Ok(table) => {
                                let mut packages = table.packages;
                                packages.retain(|package| {
                                    !scooped
                                        .iter()
                                        .any(|local| local.id.eq_ignore_ascii_case(&package.id))
                                });
                                if !updates {
                                    packages.append(&mut scooped);
                                }
                                Ok(Response::Inventory(packages, table.skipped, updates, None))
                            }
                            Err(error) if !updates && !scooped.is_empty() => Ok(
                                Response::Inventory(scooped, 0, false, Some(error.to_string())),
                            ),
                            Err(error) => Err(error),
                        }
                    }
                    ReadKind::Details(mut package) => {
                        if let Some(row) = package.scoop_catalog.as_deref() {
                            package.installed = scoop
                                .as_ref()
                                .and_then(|scoop| scoop.catalog_installed(&row.bucket, &row.name));
                            let block = if package.installed.is_some() {
                                Some("Installed through Scoop. Use Scoop for updates or removal.")
                            } else {
                                scoop.as_ref().map_or(
                                    Some("Set up Scoop to continue with installation."),
                                    |scoop| scoop.review_block(row),
                                )
                            };
                            return Ok(Response::Details(package, block.unwrap_or("Review the exact Scoop manifest and download before installing.").into(), block.is_none()));
                        }
                        if package.manageable() || package.source == scoop::SOURCE {
                            if let Some(installed) = scoop
                                .as_ref()
                                .and_then(|scoop| scoop.installed_package(&package.id))
                            {
                                return Ok(Response::Details(
                                    installed,
                                    "Installed through Scoop. Use Scoop for updates or removal."
                                        .into(),
                                    true,
                                ));
                            }
                            if package.source == scoop::SOURCE {
                                package.installed = None;
                                return Ok(Response::Details(package, "The Scoop installation could not be verified. Refresh Installed Apps.".into(), false));
                            }
                        }
                        // Details show a live catalog version, independent of the browsing snapshot.
                        package.version.clear();
                        let details = winget
                            .read(
                                &Request::Details(package.id.clone()),
                                job.source_consent,
                                obsolete,
                            )?
                            .checked()?
                            .text;
                        let installed = winget::parse_table(
                            &winget.read(
                                &Request::InstalledExact(package.id.clone(), None),
                                job.source_consent,
                                obsolete,
                            )?,
                            true,
                        )?;
                        let verified_inventory = installed.skipped == 0 && !installed.ambiguous;
                        if let Some(found) =
                            installed.packages.iter().find(|p| p.key() == package.key())
                        {
                            package.installed = found.installed.clone();
                            package.available = found.available.clone();
                        } else {
                            package.installed = None;
                            package.available = None;
                        }
                        if let Ok(output) = winget.read(
                            &Request::CatalogExact(package.id.clone()),
                            job.source_consent,
                            obsolete,
                        ) && let Ok(table) = winget::parse_table(&output, false)
                            && let Some(entry) =
                                table.packages.iter().find(|p| p.key() == package.key())
                        {
                            package.version.clone_from(&entry.version);
                        }
                        Ok(Response::Details(package, details, verified_inventory))
                    }
                    ReadKind::Batch(action, selection) => {
                        let mut eligible = Vec::new();
                        let mut skipped = Vec::new();
                        for package in selection {
                            if obsolete() {
                                break;
                            }
                            let review = || {
                                if !matches!(
                                    action,
                                    BatchAction::Install | BatchAction::InstallScoop
                                ) {
                                    return winget.review_package(
                                        action.verb(),
                                        &package,
                                        job.source_consent,
                                        obsolete,
                                    );
                                }
                                if let Some(scoop) = &scoop {
                                    scoop.check_absent(&package.id)?;
                                }
                                if matches!(action, BatchAction::Install)
                                    && winget.executable.is_some()
                                    && let Some(reviewed) = winget.review_install(
                                        &package,
                                        job.source_consent,
                                        obsolete,
                                    )?
                                {
                                    return Ok(reviewed);
                                }
                                let scoop = scoop.as_ref().ok_or_else(|| AppError(
                                    if winget.executable.is_some() {
                                        "WinGet has no exact catalog match and Scoop is unavailable."
                                    } else {
                                        "WinGet and Scoop are unavailable."
                                    }.into()
                                ))?;
                                scoop.review(&winget, &package, job.source_consent, obsolete)
                            };
                            match review() {
                                Ok(reviewed) => eligible.push(reviewed),
                                Err(reason) => skipped.push(format!(
                                    "{}: {}",
                                    package.id,
                                    reason
                                        .0
                                        .lines()
                                        .next()
                                        .unwrap_or("Could not verify package"),
                                )),
                            }
                        }
                        Ok(Response::Batch(action, eligible, skipped))
                    }
                    ReadKind::Sources => {
                        let result = winget.read(&Request::Sources, false, obsolete)?.checked()?;
                        Ok(Response::Sources(winget::source_summary(&result.text)))
                    }
                }
            })();
            if obsolete() {
                continue;
            }
            post(
                &ui,
                Some(job.generation),
                result.unwrap_or_else(|e| {
                    if diagnostic {
                        Response::BackendError(e.to_string())
                    } else {
                        Response::Error(e.to_string())
                    }
                }),
            );
        }
    })
}

fn catalog_worker(
    storage: Storage,
    epoch: Arc<AtomicU64>,
    ui: slint::Weak<MainWindow>,
) -> (mpsc::Sender<CatalogJob>, std::thread::JoinHandle<()>) {
    let (sender, receiver) = mpsc::channel::<CatalogJob>();
    let worker = std::thread::spawn(move || {
        let mut catalog = scoop_catalog::Catalog::default();
        while let Ok(job) = receiver.recv() {
            let active = || epoch.load(Ordering::Relaxed) == job.generation;
            if !active() {
                continue;
            }
            let scoop = Scoop::detect();
            let result = catalog.search(
                &job.query,
                scoop.as_ref(),
                job.persist.then_some(&storage),
                job.additional,
                job.force,
                active,
            );
            if active() {
                post(&ui, Some(job.generation), Response::ScoopSearch(result));
            }
        }
    });
    (sender, worker)
}

fn install_page(page: i32) -> bool {
    matches!(page, 0..=2 | 8)
}

pub(crate) const INSTALL_QUEUE_PAGE: i32 = 9;

#[derive(Clone)]
struct View {
    details: Option<Package>,
    page: i32,
    query: String,
    category: i32,
    sort: i32,
    collection: i32,
    tag_category: Option<String>,
    packages: Vec<Package>,
    heading: String,
    subtitle: String,
    scoop_searched: bool,
    winget_search_note: String,
}
enum Pending {
    Changes(Vec<(Package, &'static str)>),
    Close,
}
struct State {
    generation: Arc<AtomicU64>,
    sender: mpsc::Sender<Job>,
    catalog: Option<(mpsc::Sender<CatalogJob>, std::thread::JoinHandle<()>)>,
    manager: Manager,
    scoop_available: bool,
    view: View,
    history: Vec<View>,
    installed: HashMap<String, Package>,
    inventory_known: bool,
    selected: HashSet<String>,
    install_selection: BTreeMap<String, Package>,
    uninstall_selection: BTreeMap<String, Package>,
    detail_ready: bool,
    cache: VecDeque<(String, (Package, String, bool))>,
    pending: Option<Pending>,
    storage: Storage,
    settings: Settings,
    icons: icons::Loader,
    search_timer: slint::Timer,
    resize_timer: slint::Timer,
    backend: String,
    detected: bool,
}

impl State {
    fn job(&mut self, ui: &MainWindow, kind: ReadKind) {
        let generation = self.generation.fetch_add(1, Ordering::Relaxed) + 1;
        ui.set_busy(true);
        let message = match &kind {
            ReadKind::Details(p) if p.source == scoop::SOURCE => "Loading Scoop package details…",
            ReadKind::Batch(BatchAction::InstallScoop, _) => "Checking the Scoop package…",
            ReadKind::DetectScoop => "Checking for Scoop…",
            _ => "Working with the system WinGet…",
        };
        ui.set_status(message.into());
        if self
            .sender
            .send(Job {
                generation,
                kind,
                source_consent: self.settings.source_consent,
            })
            .is_err()
        {
            ui.set_busy(false);
            ui.set_status("The WinGet worker stopped. Restart SlimStore.".into());
        }
    }
    fn cancel_read(&self, ui: &MainWindow) {
        self.generation.fetch_add(1, Ordering::Relaxed);
        ui.set_busy(false);
    }
    fn remember(&mut self) {
        if self.history.len() >= 25 {
            self.history.remove(0);
        }
        self.history.push(self.view.clone());
    }
    fn search_scoop(&mut self, ui: &MainWindow, force: bool) {
        if self.view.page != 8 || self.view.query.is_empty() {
            return;
        }
        self.view.scoop_searched = true;
        self.view.subtitle = format!("{} · Searching Scoop…", self.view.winget_search_note);
        let (sender, _) = self.catalog.get_or_insert_with(|| {
            catalog_worker(self.storage.clone(), self.generation.clone(), ui.as_weak())
        });
        let generation = self.generation.fetch_add(1, Ordering::Relaxed) + 1;
        if sender
            .send(CatalogJob {
                generation,
                query: self.view.query.clone(),
                force,
                persist: self.settings.scoop_catalog_cache,
                additional: self.settings.scoop_additional_buckets,
            })
            .is_ok()
        {
            ui.set_busy(true);
            ui.set_status("Searching the Scoop catalog…".into());
        } else {
            ui.set_busy(false);
            ui.set_status("The Scoop catalog worker stopped. Restart SlimStore.".into());
        }
    }
}

fn item(package: &Package, state: &State, known: bool) -> AppItem {
    let curated = matches!(package.source.as_str(), winget::SOURCE | scoop::SOURCE)
        .then(|| metadata::curated(&package.id))
        .flatten();
    let key = package.key();
    let inventory = state.installed.get(&key).or_else(|| {
        (package.source == winget::SOURCE)
            .then(|| format!("scoop:{}", package.id.to_ascii_lowercase()))
            .and_then(|key| state.installed.get(&key))
    });
    let installed = package
        .installed
        .as_ref()
        .or_else(|| inventory.and_then(|p| p.installed.as_ref()));
    let available = package
        .available
        .as_ref()
        .or_else(|| inventory.and_then(|p| p.available.as_ref()));
    let (status, status_version) = match (installed, available) {
        (Some(current), Some(available)) => {
            ("Update available", format!("{current} → {available}"))
        }
        (Some(current), None) => ("Installed", format!("Version {current}")),
        (None, _) => (
            if known {
                "Not installed"
            } else {
                "Status unknown"
            },
            if package.version.is_empty() {
                String::new()
            } else {
                format!("Catalog {}", package.version)
            },
        ),
    };
    let installed_version = installed.cloned().unwrap_or_default();
    let available_version = available.cloned().unwrap_or_default();
    let hash = package
        .id
        .bytes()
        .fold(0usize, |a, b| a.wrapping_mul(31).wrapping_add(b as usize));
    let colors = [
        (89, 92, 210),
        (56, 133, 167),
        (152, 88, 159),
        (53, 144, 124),
        (188, 111, 75),
        (100, 111, 145),
    ];
    let (r, g, b) = colors[hash % colors.len()];
    let scoop_row = package.scoop_catalog.as_deref();
    AppItem {
        id: package.id.clone().into(),
        name: package.name.clone().into(),
        publisher: scoop_row
            .map_or_else(
                || curated.map_or("", |p| p.publisher.as_str()).to_owned(),
                |row| format!("Scoop · {}", row.bucket),
            )
            .into(),
        publisher_description: scoop_row
            .map_or_else(
                || curated.map_or("", |p| p.description.as_str()),
                |row| row.description.as_str(),
            )
            .into(),
        summary: curated.map_or("", |p| p.summary.as_str()).into(),
        description: curated
            .map_or(scoop_row.map_or("", |row| row.description.as_str()), |p| {
                if p.summary.is_empty() {
                    &p.description
                } else {
                    &p.summary
                }
            })
            .into(),
        license: curated
            .map_or(scoop_row.map_or("", |row| row.license.as_str()), |p| {
                p.license.as_str()
            })
            .into(),
        website: curated
            .map_or(scoop_row.map_or("", |row| row.homepage.as_str()), |p| {
                p.website.as_str()
            })
            .into(),
        notes: curated.map_or("", |p| p.notes.as_str()).into(),
        art_id: curated.map_or("", |p| p.id.as_str()).into(),
        icon_slot: state.icons.slot(package),
        installed_version: installed_version.into(),
        available_version: available_version.into(),
        source: package.source.clone().into(),
        status: status.into(),
        status_version: status_version.into(),
        initials: metadata::initials(&package.name).into(),
        category: metadata::category(&package.id).into(),
        selected: match state.view.page {
            3 => state.uninstall_selection.contains_key(&key),
            4 => state.selected.contains(&key),
            _ => state.install_selection.contains_key(&key),
        },
        actionable: match state.view.page {
            3 => package.manageable() && package.installed.is_some(),
            4 => package.update_eligible(),
            page => {
                install_page(page)
                    && package.manageable()
                    && (installed.is_none() || state.install_selection.contains_key(&key))
            }
        },
        tint: slint::Color::from_rgb_u8(r, g, b),
    }
}

fn render(ui: &MainWindow, state: &State) {
    state.icons.begin_view();
    let v = &state.view;
    ui.set_page(v.page);
    ui.set_install_count(state.install_selection.len() as i32);
    ui.set_uninstall_count(state.uninstall_selection.len() as i32);
    ui.set_update_count(
        v.packages
            .iter()
            .filter(|p| p.update_eligible() && state.selected.contains(&p.key()))
            .count() as i32,
    );
    ui.set_heading(v.heading.clone().into());
    ui.set_subtitle(v.subtitle.clone().into());
    ui.set_scoop_search_available(
        v.page == 8 && !v.scoop_searched && !v.winget_search_note.is_empty(),
    );
    ui.set_scoop_detected(state.scoop_available);
    ui.set_category_index(v.category);
    ui.set_sort_index(v.sort);
    ui.set_collection_index(v.collection);
    ui.set_inventory_known(state.inventory_known);
    ui.set_can_back(!state.history.is_empty());
    let category = CATEGORIES
        .get(v.category as usize)
        .copied()
        .unwrap_or(CATEGORIES[0]);
    let mut packages: Vec<_> = if v.page == INSTALL_QUEUE_PAGE {
        state.install_selection.values().collect()
    } else {
        v.packages
            .iter()
            .filter(|p| {
                (v.category == 0
                    || metadata::category(&p.id) == category
                    || v.tag_category.as_deref() == Some(category))
                    && metadata::in_collection(&p.id, v.collection.max(0) as usize)
            })
            .collect()
    };
    if v.sort != 0 {
        packages.sort_by_cached_key(|p| p.name.to_lowercase());
        if v.sort == 2 {
            packages.reverse();
        }
    }
    let columns = ui.get_columns().max(1) as usize;
    let rows: Vec<AppRow> = packages
        .chunks(columns)
        .map(|chunk| AppRow {
            first: item(chunk[0], state, state.inventory_known),
            second: chunk
                .get(1)
                .map(|p| item(p, state, state.inventory_known))
                .unwrap_or_default(),
            third: chunk
                .get(2)
                .map(|p| item(p, state, state.inventory_known))
                .unwrap_or_default(),
        })
        .collect();
    ui.set_rows(ModelRc::new(VecModel::from(rows)));
    ui.set_result_count(packages.len() as i32);
    let operations = state.manager.snapshot();
    ui.set_operation_count(state.manager.pending() as i32);
    let block = state.manager.block();
    ui.set_queue_block(
        match block {
            Some(QueueBlock::InstallerBusy) => {
                "Queue paused. Wait for the other installer to finish, then resume."
            }
            Some(QueueBlock::RestartRequired) => {
                "Windows needs to restart. Review the remaining changes after restarting."
            }
            None => "",
        }
        .into(),
    );
    ui.set_queue_resumable(
        block == Some(QueueBlock::InstallerBusy) && !state.manager.is_stopping(),
    );
    let activity: Vec<ActivityItem> = operations
        .into_iter()
        .rev()
        .map(|e| ActivityItem {
            key: e.serial.to_string().into(),
            title: if e.package.scoop.is_some() {
                format!("{} · {} via Scoop", e.package.name, e.verb)
            } else {
                format!("{} · {}", e.package.name, e.verb)
            }
            .into(),
            status: e.status.label().into(),
            summary: e
                .diagnostic
                .lines()
                .find(|line| !line.trim().is_empty())
                .unwrap_or_default()
                .into(),
            detail: e.diagnostic.into(),
            cancellable: e.status == Status::Queued,
            reviewable: e.status.reviewable() && block.is_none(),
        })
        .collect();
    ui.set_activity(ModelRc::new(VecModel::from(activity)));
}

fn show_detail(ui: &MainWindow, state: &mut State, package: Package, text: String, ready: bool) {
    if ready {
        let key = package.key();
        if package.installed.is_some() {
            state.installed.insert(key, package.clone());
        } else {
            state.installed.remove(&key);
        }
    }
    let mut detail = item(&package, state, ready);
    // A live catalog lookup supplies the latest version separately from any
    // version reported by the installed inventory.
    if detail.available_version.is_empty() {
        detail.available_version = package.version.clone().into();
    }
    ui.set_detail(detail);
    ui.set_detail_installed(package.installed.is_some());
    ui.set_detail_action(
        if package.installed.is_none() {
            "Install"
        } else if package.update_eligible() {
            "Update"
        } else {
            "Installed"
        }
        .into(),
    );
    let scoop_result = package.scoop_catalog.is_some();
    ui.set_detail_actionable(ready && (package.manageable() || scoop_result));
    ui.set_detail_scoop_result(scoop_result);
    ui.set_detail_scoop_manifest(
        package
            .scoop_catalog
            .as_deref()
            .is_some_and(|row| row.manifest_url().is_some()),
    );
    ui.set_detail_scoop_note(if scoop_result {
        text.clone().into()
    } else {
        "".into()
    });
    ui.set_detail_ready(ready);
    let inventory = format!(
        "Source: {}\nInstalled version: {}\nAvailable update: {}\n\n{}\n\n{}",
        package.source,
        package.installed.as_deref().unwrap_or(if scoop_result {
            if ready {
                "No matching Scoop installation"
            } else {
                "Unknown"
            }
        } else if ready {
            "Not installed (verified by WinGet)"
        } else {
            "Unknown"
        }),
        package.available.as_deref().unwrap_or("None reported"),
        if scoop_result {
            "Catalog metadata comes from the Scoop bucket. Install review checks the exact official manifest."
        } else if ready {
            if package.source == scoop::SOURCE {
                "Installed state is from Scoop's local records."
            } else {
                "Information below is returned verbatim by WinGet in the system language. Availability and installer compatibility are determined by WinGet."
            }
        } else {
            "Package management is disabled because installed state could not be verified."
        },
        text
    );
    ui.set_detail_text(inventory.into());
    state.view.details = Some(package);
    state.detail_ready = ready;
}

fn receive(ui: &MainWindow, state: &Rc<RefCell<State>>, generation: Option<u64>, event: Response) {
    let mut s = state.borrow_mut();
    let diagnostic = matches!(&event, Response::Detection(..) | Response::BackendError(..));
    let current = generation.is_none_or(|g| g == s.generation.load(Ordering::Relaxed));
    if !current && !diagnostic {
        return;
    }
    if generation.is_some() && current {
        ui.set_busy(false);
    }
    match event {
        Response::BackendError(error) => {
            s.backend = error.clone();
            ui.set_backend_status(error.clone().into());
            if current {
                ui.set_status(error.into());
            }
        }
        Response::Detection(version, sources) => {
            s.detected = true;
            s.backend = version.clone();
            ui.set_backend_status(version.into());
            ui.set_sources(sources.into());
            if current {
                ui.set_status("Ready".into());
            }
            if install_page(s.view.page) && !ui.get_busy() && !s.inventory_known {
                s.job(ui, ReadKind::Inventory(false));
            }
        }
        Response::Sources(sources) => {
            ui.set_sources(sources.into());
            ui.set_status("Configured sources refreshed".into());
        }
        Response::WinGetSearch(result) => {
            let search_scoop = match result {
                Ok(table) => {
                    let empty = table.packages.is_empty() && table.skipped == 0 && !table.ambiguous;
                    let count = table.packages.len();
                    s.view.packages = table.packages;
                    s.view.winget_search_note = if empty {
                        "No WinGet matches".into()
                    } else if table.skipped > 0 {
                        format!(
                            "{count} WinGet results · {} unsupported rows hidden",
                            table.skipped
                        )
                    } else {
                        format!("{count} WinGet results")
                    };
                    empty
                }
                Err(error) => {
                    s.view.winget_search_note = if s.view.packages.is_empty() {
                        "WinGet search unavailable".into()
                    } else {
                        "WinGet search unavailable · showing embedded catalog matches".into()
                    };
                    ui.set_status(
                        error
                            .0
                            .lines()
                            .next()
                            .unwrap_or("WinGet search unavailable")
                            .into(),
                    );
                    false
                }
            };
            s.view.subtitle = s.view.winget_search_note.clone();
            if search_scoop || s.view.scoop_searched {
                s.search_scoop(ui, false);
            } else if !s
                .view
                .winget_search_note
                .starts_with("WinGet search unavailable")
            {
                ui.set_status(s.view.subtitle.clone().into());
            }
        }
        Response::ScoopSearch(result) => {
            s.view.packages.retain(|p| p.scoop_catalog.is_none());
            let count = result.packages.len();
            let mut note = if result.complete {
                if result.total > count {
                    format!("Showing {count} of {} Scoop matches", result.total)
                } else {
                    format!("{count} Scoop matches")
                }
            } else if count > 0 {
                format!("{count} Scoop matches · some buckets unavailable")
            } else {
                "Scoop catalog incomplete · Refresh to try again".into()
            };
            if result.cached {
                note.push_str(" · using stored catalog data");
            }
            s.view.packages.extend(result.packages);
            s.view.subtitle = format!("{} · {note}", s.view.winget_search_note);
            ui.set_status(note.into());
        }
        Response::ScoopDetection(available) => {
            s.scoop_available = available;
            s.cache.retain(|(_, (p, _, _))| p.scoop_catalog.is_none());
            set_scoop_status(ui, available);
            let artwork = ui.global::<Artwork>();
            artwork.invoke_scoop_changed(available);
            artwork.set_local_enabled(available);
            ui.set_status(
                if available {
                    "Scoop detected"
                } else {
                    "Scoop was not found"
                }
                .into(),
            );
            if s.view.page == 7
                && let Some(p) = s.view.details.clone()
                && p.scoop_catalog.is_some()
            {
                s.job(ui, ReadKind::Details(p));
            }
        }
        Response::Packages(packages, skipped, category) => {
            s.view.packages = packages;
            if s.view.page == 8 {
                s.view.subtitle = "Live results from the WinGet community source".into();
            }
            s.view.tag_category = category;
            ui.set_status(
                format!(
                    "{} apps returned by WinGet{}",
                    s.view.packages.len(),
                    if skipped > 0 {
                        format!(" · {skipped} unsupported rows hidden")
                    } else {
                        String::new()
                    }
                )
                .into(),
            );
        }
        Response::Inventory(packages, skipped, updates, warning) => {
            if !updates {
                s.installed = packages.iter().map(|p| (p.key(), p.clone())).collect();
                s.inventory_known = skipped == 0 && warning.is_none();
                if s.inventory_known {
                    let s = &mut *s;
                    s.uninstall_selection
                        .retain(|key, _| s.installed.contains_key(key));
                }
            } else {
                for package in s.installed.values_mut() {
                    package.available = None;
                }
                for p in &packages {
                    s.installed.insert(p.key(), p.clone());
                }
            }
            let count = packages.len();
            let source = if warning.is_some() {
                "Scoop"
            } else if packages.iter().any(|p| p.source == scoop::SOURCE) {
                "WinGet and Scoop"
            } else {
                "WinGet"
            };
            if s.view.page == if updates { 4 } else { 3 } {
                s.view.packages = packages;
            }
            ui.set_status(
                format!(
                    "{} recognized {} · source: {}{}",
                    count,
                    if updates { "updates" } else { "installed apps" },
                    source,
                    if skipped > 0 {
                        format!(" · {skipped} unsupported rows hidden")
                    } else if warning.is_some() {
                        " · WinGet inventory unavailable".into()
                    } else {
                        String::new()
                    }
                )
                .into(),
            );
        }
        Response::Details(package, text, ready) => {
            let key = package.key();
            s.cache.retain(|(k, _)| k != &key);
            s.cache
                .push_back((key, (package.clone(), text.clone(), ready)));
            while s.cache.len() > 48 {
                s.cache.pop_front();
            }
            show_detail(ui, &mut s, package, text, ready);
            ui.set_status(
                if ui.get_detail_scoop_result() {
                    "Scoop package details loaded"
                } else {
                    "Package details and installation state checked with WinGet"
                }
                .into(),
            );
        }
        Response::Batch(action, eligible, skipped) => {
            if eligible.is_empty() {
                let reason = skipped
                    .first()
                    .map_or("No packages were verified.", String::as_str);
                ui.set_status(format!("Nothing to {}. {reason}", action.verb()).into());
            } else {
                let count = eligible.len();
                let changes = eligible.into_iter().map(|p| (p, action.verb())).collect();
                show_confirmation(ui, &mut s, changes, &skipped);
                ui.set_status(
                    format!(
                        "{count} selected packages verified. Review before you {}.",
                        action.verb()
                    )
                    .into(),
                );
            }
        }
        Response::Error(error) => {
            ui.set_status(
                error
                    .lines()
                    .next()
                    .unwrap_or("WinGet request failed")
                    .into(),
            );
            if s.view.page == 7 {
                ui.set_detail_ready(false);
                ui.set_detail_text(format!("Could not verify this package with WinGet.\n\n{error}\n\nReview source agreements in Settings, check connectivity, then refresh.").into());
                s.detail_ready = false;
            } else if s.view.page == 8 {
                s.view.subtitle =
                    "WinGet search unavailable. Showing matches from the embedded catalog.".into();
            } else if [3, 4].contains(&s.view.page) {
                s.view.packages.clear();
                s.view.subtitle =
                    "Could not load the WinGet inventory. See the status below and refresh.".into();
            } else if !s.detected {
                s.backend = error.clone();
                ui.set_backend_status(error.into());
            }
        }
        Response::Operations(finished) => {
            if s.manager.is_stopping() {
                if finished {
                    ui.set_status("Active operation finished. SlimStore can now close.".into());
                }
                render(ui, &s);
                return;
            }
            if finished {
                s.cache.clear();
                s.inventory_known = false;
                s.installed.clear();
                if ui.get_busy() || ui.get_confirm_visible() {
                    // Preserve an active search or package review. Changes are rechecked at launch.
                    render(ui, &s);
                    return;
                }
                ui.set_status("Operation finished. Refreshing package state…".into());
                match s.view.page {
                    page if install_page(page) => s.job(ui, ReadKind::Inventory(false)),
                    3 => s.job(ui, ReadKind::Inventory(false)),
                    4 => s.job(ui, ReadKind::Inventory(true)),
                    7 => {
                        if let Some(p) = s.view.details.clone() {
                            ui.set_detail_ready(false);
                            s.detail_ready = false;
                            s.job(ui, ReadKind::Details(p));
                        }
                    }
                    _ => ui.set_status(
                        "Operation finished · refresh your library to check the current state"
                            .into(),
                    ),
                }
            }
        }
    }
    render(ui, &s);
}

fn navigate(ui: &MainWindow, state: &Rc<RefCell<State>>, page: i32) {
    let mut s = state.borrow_mut();
    if s.view.page == page {
        return;
    }
    s.search_timer.stop();
    s.remember();
    s.cancel_read(ui);
    s.view.page = page;
    ui.set_results_y(0.);
    s.view.category = 0;
    s.view.collection = 0;
    s.view.sort = 0;
    s.view.tag_category = None;
    s.view.query.clear();
    ui.set_query("".into());
    let (heading, subtitle) = match page {
        0 => (
            "Featured & essential apps",
            "Editorial picks from the WinGet community catalog",
        ),
        1 => (
            "Discover apps",
            "Browse the editorial catalog by collection or category.",
        ),
        2 => (
            "Categories",
            "Choose a category to browse the editorial catalog or look up its WinGet tag.",
        ),
        3 => (
            "Your installed apps",
            "Packages recognized by WinGet and supported existing Scoop installs. Other sources are outside this inventory.",
        ),
        4 => (
            "Keep good things current",
            "Updates reported by WinGet. Select eligible apps, or review all before updating.",
        ),
        INSTALL_QUEUE_PAGE => (
            "Install Queue",
            "Review the apps you selected for installation.",
        ),
        _ => ("", ""),
    };
    s.view.heading = heading.into();
    s.view.subtitle = subtitle.into();
    if page <= 2 {
        s.view.packages = metadata::initial_packages(page == 0);
        if s.detected && !s.inventory_known && s.installed.is_empty() {
            s.job(ui, ReadKind::Inventory(false));
        }
    }
    if page == 3 || page == 4 {
        s.view.packages.clear();
        s.job(ui, ReadKind::Inventory(page == 4));
    }
    if page == INSTALL_QUEUE_PAGE {
        s.view.packages.clear();
        if s.detected {
            ui.set_status("Ready".into());
        }
    }
    if page == 6 {
        s.job(ui, ReadKind::Sources);
    }
    render(ui, &s);
}

fn refresh(ui: &MainWindow, state: &Rc<RefCell<State>>) {
    let mut s = state.borrow_mut();
    match s.view.page {
        3 => s.job(ui, ReadKind::Inventory(false)),
        4 => s.job(ui, ReadKind::Inventory(true)),
        6 => s.job(ui, ReadKind::Sources),
        2 if s.view.tag_category.is_some() => {
            if let Some(category) = s.view.tag_category.clone() {
                s.job(ui, ReadKind::Category(category));
            }
        }
        7 => {
            if let Some(p) = s.view.details.clone() {
                s.cache.retain(|(k, _)| k != &p.key());
                s.detail_ready = false;
                ui.set_detail_ready(false);
                s.job(ui, ReadKind::Details(p));
            }
        }
        8 => {
            if s.view.scoop_searched {
                s.search_scoop(ui, true);
            } else {
                let query = s.view.query.clone();
                s.job(ui, ReadKind::Search(query));
            }
        }
        _ => {
            if !s.detected {
                s.job(ui, ReadKind::Detect);
            } else {
                s.job(ui, ReadKind::Inventory(false));
            }
        }
    }
}

fn open_app(ui: &MainWindow, state: &Rc<RefCell<State>>, id: &str) {
    let mut s = state.borrow_mut();
    let package = if s.view.page == INSTALL_QUEUE_PAGE {
        s.install_selection.values().find(|p| p.id == id)
    } else {
        s.view.packages.iter().find(|p| p.id == id)
    };
    let Some(package) = package.cloned() else {
        return;
    };
    s.search_timer.stop();
    s.remember();
    s.cancel_read(ui);
    s.view.page = 7;
    ui.set_details_expanded(false);
    ui.set_preview_index(0);
    ui.set_detail_action("Install".into());
    ui.set_detail(item(&package, &s, false));
    ui.set_detail_ready(false);
    ui.set_detail_installed(false);
    ui.set_detail_scoop_result(package.scoop_catalog.is_some());
    ui.set_detail_scoop_manifest(
        package
            .scoop_catalog
            .as_deref()
            .is_some_and(|row| row.manifest_url().is_some()),
    );
    ui.set_detail_scoop_note("".into());
    ui.set_detail_text("Loading package details…".into());
    s.view.details = Some(package.clone());
    s.detail_ready = false;
    if let Some((_, (package, text, ready))) = s
        .cache
        .iter()
        .find(|(key, _)| key == &package.key())
        .cloned()
    {
        show_detail(ui, &mut s, package, text, ready);
    } else {
        s.job(ui, ReadKind::Details(package));
    }
    render(ui, &s);
}

fn prepare_action(ui: &MainWindow, state: &Rc<RefCell<State>>, action: &str) {
    if ui.get_confirm_visible() {
        return;
    }
    let mut s = state.borrow_mut();
    if s.manager.is_stopping() {
        ui.set_status("SlimStore is closing. Reopen it to start new operations.".into());
        return;
    }
    match action {
        "search-scoop" => {
            if !ui.get_busy() {
                s.search_scoop(ui, false);
                render(ui, &s);
            }
            return;
        }
        "detect-scoop" => {
            if !ui.get_busy() {
                s.job(ui, ReadKind::DetectScoop);
            }
            return;
        }
        "scoop-instructions" => {
            if let Some(row) = s
                .view
                .details
                .as_ref()
                .and_then(|p| p.scoop_catalog.as_deref())
            {
                let extras = if row.bucket == "extras" {
                    "\n\nIf Extras is missing, use scoop bucket add extras. Scoop requires Git to add buckets."
                } else {
                    ""
                };
                platform::information(
                    &format!(
                        "Open PowerShell under your usual Windows account. Review the package and its bucket before running this command.\n\nscoop install {}{extras}\n\nScoop manages the installation and subsequent updates.",
                        row.qualified()
                    ),
                    false,
                );
            }
            return;
        }
        "scoop-manifest" => {
            if let Some(url) = s
                .view
                .details
                .as_ref()
                .and_then(|p| p.scoop_catalog.as_deref())
                .and_then(|row| row.manifest_url())
            {
                platform::open(&url);
            }
            return;
        }
        "clear-selection" => {
            if !ui.get_busy() {
                if s.view.page == 3 {
                    s.uninstall_selection.clear();
                } else if install_page(s.view.page) || s.view.page == INSTALL_QUEUE_PAGE {
                    s.install_selection.clear();
                    if s.view.page == INSTALL_QUEUE_PAGE {
                        ui.set_status("Selection cleared".into());
                    }
                }
                render(ui, &s);
            }
            return;
        }
        "install-selected" | "uninstall-selected" => {
            let (verb, selection) = match action {
                "install-selected" if s.view.page == INSTALL_QUEUE_PAGE => {
                    (BatchAction::Install, &s.install_selection)
                }
                "uninstall-selected" if s.view.page == 3 => {
                    (BatchAction::Uninstall, &s.uninstall_selection)
                }
                _ => return,
            };
            if !ui.get_busy() && !selection.is_empty() {
                let count = selection.len();
                let packages = selection.values().cloned().collect();
                s.job(ui, ReadKind::Batch(verb, packages));
                ui.set_status(format!("Checking {count} selected packages with WinGet…").into());
            }
            return;
        }
        _ => {}
    }
    if ui.get_busy() {
        return;
    }
    let (intent, packages) = if s.view.page == 4 && matches!(action, "all" | "selected") {
        let packages = s
            .view
            .packages
            .iter()
            .filter(|p| p.update_eligible() && (action == "all" || s.selected.contains(&p.key())))
            .cloned()
            .collect();
        (BatchAction::Upgrade, packages)
    } else if s.view.page == 7
        && s.detail_ready
        && let Some(p) = s.view.details.as_ref()
    {
        let intent = match action {
            "uninstall" if p.installed.is_some() => BatchAction::Uninstall,
            "primary" if p.scoop_catalog.is_some() && p.installed.is_none() => {
                BatchAction::InstallScoop
            }
            "primary" if p.installed.is_none() => BatchAction::Install,
            "primary" if p.update_eligible() => BatchAction::Upgrade,
            _ => return,
        };
        (intent, vec![p.clone()])
    } else {
        return;
    };
    if packages.is_empty() {
        ui.set_status(
            "No eligible applications selected. Installed apps without updates need no action."
                .into(),
        );
        return;
    }
    let count = packages.len();
    s.job(ui, ReadKind::Batch(intent, packages));
    ui.set_status(format!("Checking {count} selected packages…").into());
}

fn show_confirmation(
    ui: &MainWindow,
    s: &mut State,
    changes: Vec<(Package, &'static str)>,
    skipped: &[String],
) {
    let requires_agreement = changes.iter().any(|(_, verb)| *verb != "uninstall");
    let apps = changes
        .iter()
        .map(|(p, v)| {
            let installed = p.installed.as_deref().unwrap_or("Unknown");
            let version = match *v {
                "upgrade" => format!(
                    "{installed} → {}",
                    p.available.as_deref().unwrap_or("Unknown")
                ),
                "uninstall" => installed.into(),
                _ => p.version.clone(),
            };
            let download = p
                .scoop
                .as_ref()
                .map_or_else(String::new, |scoop| format!("\n  Download: {}", scoop.url));
            format!(
                "{} {}\n  {} · source: {}\n  Version: {version}{download}",
                v,
                p.name,
                p.id,
                p.reviewed_source()
            )
        })
        .collect::<Vec<_>>()
        .join("\n\n");
    let intent = match changes[0].1 {
        "install" => "Install",
        "uninstall" => "Uninstall",
        _ => "Update",
    };
    let title = if changes.len() == 1 {
        format!("{intent} {}?", changes[0].0.name)
    } else {
        format!("{intent} {} applications?", changes.len())
    };
    ui.set_confirm_title(title.into());
    let exclusions = if skipped.is_empty() {
        String::new()
    } else {
        format!(
            "\n\nSkipped {} selected packages:\n{}",
            skipped.len(),
            skipped.join("\n")
        )
    };
    let guidance = if requires_agreement {
        "Review the publisher licenses and agreements in Details. An installer may ask for elevation or require a restart."
    } else {
        "This removes the listed applications from your PC. An uninstaller may ask for elevation or require a restart."
    };
    let files_in_use = if changes.iter().any(|(_, verb)| *verb != "install") {
        "\n\nSave your work and close the listed apps before continuing. Publisher installers can close running apps."
    } else {
        ""
    };
    let scoop_guidance = if changes.iter().any(|(p, _)| p.scoop.is_some()) {
        "\n\nScoop installs for the current user. Check for copies installed outside the selected package manager before confirming."
    } else {
        ""
    };
    ui.set_confirm_text(format!("{apps}{exclusions}\n\n{guidance}{files_in_use}{scoop_guidance}\n\nOperations run one at a time. You can cancel queued operations.").into());
    ui.set_confirm_agreement_required(requires_agreement);
    ui.set_confirm_information(false);
    ui.set_package_consent(false);
    ui.set_confirm_visible(true);
    s.pending = Some(Pending::Changes(changes));
}

fn apply_settings(ui: &MainWindow, s: &mut State, save: bool) {
    s.settings.theme = ui.get_theme_index().clamp(0, 2) as u8;
    s.settings.accent = ui.get_accent_enabled();
    s.settings.source_consent = ui.get_source_consent();
    let online = ui.get_online_icons();
    s.settings.scoop_catalog_cache = ui.get_scoop_catalog_cache();
    s.settings.scoop_additional_buckets = ui.get_scoop_additional_buckets();
    s.settings.offline_icons_only = !online;
    s.icons.enabled.store(online, Ordering::Relaxed);
    ui.global::<Artwork>().set_online_enabled(online);
    ui.set_dark(match s.settings.theme {
        1 => false,
        2 => true,
        _ => platform::system_dark(),
    });
    ui.set_accent(if s.settings.accent {
        platform::accent().unwrap_or(slint::Color::from_rgb_u8(113, 99, 237))
    } else {
        slint::Color::from_rgb_u8(113, 99, 237)
    });
    if save {
        match s.storage.save(&s.settings, ui.get_persistent()) {
            Ok(text) => ui.set_storage_status(text.into()),
            Err(error) => {
                ui.set_storage_status(error.into());
                ui.set_persistent(false);
            }
        }
    }
}

fn set_scoop_status(ui: &MainWindow, available: bool) {
    ui.set_scoop_status(
        if available {
            "Existing Scoop detected. Supported packages can be reviewed for a per-user install."
        } else {
            "Scoop is available for search. Set it up to install Scoop packages."
        }
        .into(),
    );
}

pub fn run() -> Result<(), Box<dyn std::error::Error>> {
    let winget = Winget::detect();
    let scoop = Scoop::detect();
    let ui = MainWindow::new()?;
    ui.set_app_version(concat!(env!("CARGO_PKG_VERSION"), ".0").into());
    let weak = ui.as_weak();
    let (sender, receiver) = mpsc::channel();
    let generation = Arc::new(AtomicU64::new(0));
    let notify_ui = weak.clone();
    let manager = Manager::start(winget.clone(), move |finished| {
        post(&notify_ui, None, Response::Operations(finished))
    });
    let executable = std::env::current_exe()?;
    let mut storage = Storage::beside(&executable);
    let settings = storage.load();
    ui.set_categories(ModelRc::new(VecModel::from(
        CATEGORIES
            .iter()
            .map(|s| SharedString::from(*s))
            .collect::<Vec<_>>(),
    )));
    ui.set_collections(ModelRc::new(VecModel::from(
        metadata::COLLECTIONS
            .iter()
            .map(|label| SharedString::from(*label))
            .collect::<Vec<_>>(),
    )));
    ui.set_persistent(storage.enabled);
    ui.set_theme_index(settings.theme.min(2) as i32);
    ui.set_source_consent(settings.source_consent);
    ui.set_online_icons(!settings.offline_icons_only);
    set_scoop_status(&ui, scoop.is_some());
    ui.set_scoop_catalog_cache(settings.scoop_catalog_cache);
    ui.set_scoop_additional_buckets(settings.scoop_additional_buckets);
    ui.set_accent_enabled(settings.accent);
    if storage.enabled {
        ui.set_storage_status(
            "Portable settings loaded from SlimStore-data beside this executable".into(),
        );
    }
    let state = Rc::new(RefCell::new(State {
        generation: generation.clone(),
        sender,
        catalog: None,
        manager,
        scoop_available: scoop.is_some(),
        icons: icons::attach(&ui, !settings.offline_icons_only, scoop.clone()),
        view: View {
            details: None,
            page: 0,
            query: String::new(),
            category: 0,
            sort: 0,
            collection: 0,
            tag_category: None,
            packages: metadata::initial_packages(true),
            heading: "Featured & essential apps".into(),
            subtitle: "Editorial picks from the WinGet community catalog".into(),
            scoop_searched: false,
            winget_search_note: String::new(),
        },
        history: vec![],
        installed: HashMap::new(),
        inventory_known: false,
        selected: HashSet::new(),
        install_selection: BTreeMap::new(),
        uninstall_selection: BTreeMap::new(),
        detail_ready: false,
        cache: VecDeque::new(),
        pending: None,
        storage,
        settings,
        search_timer: slint::Timer::default(),
        resize_timer: slint::Timer::default(),
        backend: String::new(),
        detected: false,
    }));
    STATE.with(|slot| *slot.borrow_mut() = Some(state.clone()));
    UI_WEAK.with(|slot| *slot.borrow_mut() = Some(weak.clone()));
    let read_handle = read_worker(winget, scoop, receiver, generation, weak.clone());
    apply_settings(&ui, &mut state.borrow_mut(), false);
    render(&ui, &state.borrow());
    state.borrow_mut().job(&ui, ReadKind::Detect);

    {
        let w = weak.clone();
        ui.on_minimize_window(move || {
            if let Some(ui) = w.upgrade() {
                crate::chrome::command(ui.window(), crate::chrome::Action::Minimize);
            }
        });
    }
    {
        let w = weak.clone();
        ui.on_maximize_window(move || {
            if let Some(ui) = w.upgrade() {
                crate::chrome::command(ui.window(), crate::chrome::Action::ToggleMaximize);
            }
        });
    }
    {
        let w = weak.clone();
        ui.on_native_theme_changed(move |dark| {
            if let Some(ui) = w.upgrade() {
                crate::chrome::update_theme(ui.window(), dark);
            }
        });
    }

    {
        let s = state.clone();
        let w = weak.clone();
        ui.on_navigate(move |page| {
            if let Some(ui) = w.upgrade()
                && !ui.get_confirm_visible()
            {
                navigate(&ui, &s, page)
            }
        });
    }
    {
        let s = state.clone();
        let w = weak.clone();
        ui.on_back(move || {
            if let Some(ui) = w.upgrade() {
                let mut s = s.borrow_mut();
                s.search_timer.stop();
                s.cancel_read(&ui);
                if let Some(view) = s.history.pop() {
                    s.view = view;
                    ui.set_results_y(0.);
                    ui.set_query(s.view.query.clone().into());
                    match s.view.page {
                        7 => {
                            if let Some(p) = s.view.details.clone() {
                                s.detail_ready = false;
                                ui.set_detail_ready(false);
                                s.job(&ui, ReadKind::Details(p));
                            }
                        }
                        3 => s.job(&ui, ReadKind::Inventory(false)),
                        4 => s.job(&ui, ReadKind::Inventory(true)),
                        page if install_page(page)
                            && s.detected
                            && !s.inventory_known
                            && s.installed.is_empty() =>
                        {
                            s.job(&ui, ReadKind::Inventory(false));
                        }
                        _ => {}
                    }
                    render(&ui, &s);
                }
            }
        });
    }
    {
        let s = state.clone();
        let w = weak.clone();
        ui.on_refresh(move || {
            if let Some(ui) = w.upgrade() {
                refresh(&ui, &s)
            }
        });
    }
    {
        let s = state.clone();
        let w = weak.clone();
        ui.on_open_app(move |id| {
            if let Some(ui) = w.upgrade() {
                open_app(&ui, &s, &id)
            }
        });
    }
    {
        let s = state.clone();
        let w = weak.clone();
        ui.on_action(move |action| {
            if let Some(ui) = w.upgrade() {
                prepare_action(&ui, &s, &action)
            }
        });
    }
    {
        let s = state.clone();
        let w = weak.clone();
        ui.on_select_app(move |id, selected| {
            if let Some(ui) = w.upgrade()
                && !ui.get_busy()
                && !ui.get_confirm_visible()
            {
                let mut s = s.borrow_mut();
                if s.view.page == INSTALL_QUEUE_PAGE {
                    if !selected {
                        s.install_selection.retain(|_, p| p.id != id.as_str());
                        ui.set_status("Removed from Install Queue".into());
                        render(&ui, &s);
                    }
                    return;
                }
                if let Some(package) = s
                    .view
                    .packages
                    .iter()
                    .find(|p| p.id == id.as_str())
                    .cloned()
                {
                    let key = package.key();
                    if s.view.page == 4 && package.update_eligible() {
                        if selected {
                            s.selected.insert(key);
                        } else {
                            s.selected.remove(&key);
                        }
                    } else if s.view.page == 3 || install_page(s.view.page) {
                        let (eligible, selection) = if s.view.page == 3 {
                            (
                                package.manageable() && package.installed.is_some(),
                                &mut s.uninstall_selection,
                            )
                        } else {
                            (
                                package.manageable()
                                    && package.installed.is_none()
                                    && !s.installed.contains_key(&key),
                                &mut s.install_selection,
                            )
                        };
                        if !selected {
                            selection.remove(&key);
                        } else if eligible {
                            if selection.len() < QUEUE_LIMIT {
                                selection.insert(key, package);
                            } else {
                                ui.set_status(
                                    format!(
                                        "A selection can include up to {QUEUE_LIMIT} packages."
                                    )
                                    .into(),
                                );
                            }
                        }
                    }
                    render(&ui, &s);
                }
            }
        });
    }
    {
        let s = state.clone();
        let w = weak.clone();
        ui.on_cancel(move |key| {
            if let Some(ui) = w.upgrade() {
                if let Ok(serial) = key.parse() {
                    s.borrow().manager.cancel(serial);
                }
                render(&ui, &s.borrow());
            }
        });
    }
    {
        let s = state.clone();
        let w = weak.clone();
        ui.on_resume_queue(move || {
            if let Some(ui) = w.upgrade()
                && !ui.get_confirm_visible()
                && s.borrow().manager.resume()
            {
                ui.set_status("Package queue resumed.".into());
                render(&ui, &s.borrow());
            }
        });
    }
    {
        let s = state.clone();
        let w = weak.clone();
        ui.on_review_operation(move |key| {
            let Some(ui) = w
                .upgrade()
                .filter(|ui| ui.get_page() == 5 && !ui.get_busy() && !ui.get_confirm_visible())
            else {
                return;
            };
            let Ok(serial) = key.parse::<u64>() else {
                return;
            };
            let mut s = s.borrow_mut();
            if s.manager.block().is_some() || s.manager.is_stopping() {
                return;
            }
            let Some(operation) = s
                .manager
                .snapshot()
                .into_iter()
                .find(|entry| entry.serial == serial && entry.status.reviewable())
            else {
                return;
            };
            let intent = match operation.verb {
                "install" => BatchAction::Install,
                "upgrade" => BatchAction::Upgrade,
                "uninstall" => BatchAction::Uninstall,
                _ => return,
            };
            s.job(&ui, ReadKind::Batch(intent, vec![operation.package]));
        });
    }
    {
        let s = state.clone();
        let w = weak.clone();
        ui.on_search(move |query| {
            if let Some(ui) = w.upgrade() {
                let mut state = s.borrow_mut();
                state.cancel_read(&ui);
                state.view.query = query.trim().to_string();
                if state.view.query.is_empty() {
                    state.search_timer.stop();
                    if state.view.page == 8 {
                        state.view.page = 1;
                        state.view.packages = metadata::initial_packages(false);
                        state.view.heading = "Discover apps".into();
                        state.view.category = 0;
                        state.view.collection = 0;
                        ui.set_results_y(0.);
                        state.view.subtitle =
                            "Browse the editorial catalog by collection or category.".into();
                        render(&ui, &state);
                    }
                    return;
                }
                let s = s.clone();
                let w = w.clone();
                state.search_timer.start(
                    slint::TimerMode::SingleShot,
                    Duration::from_millis(380),
                    move || {
                        if let Some(ui) = w.upgrade() {
                            let mut s = s.borrow_mut();
                            if s.view.page != 8 {
                                s.remember();
                            }
                            s.view.page = 8;
                            s.view.scoop_searched = false;
                            s.view.winget_search_note.clear();
                            s.view.heading = format!("Results for “{}”", s.view.query);
                            s.view.subtitle =
                                "Searching WinGet. Embedded catalog matches appear first.".into();
                            s.view.category = 0;
                            s.view.collection = 0;
                            let query = s.view.query.clone();
                            let needle = query.to_lowercase();
                            s.view.packages = metadata::initial_packages(false)
                                .into_iter()
                                .filter(|p| {
                                    p.id.to_lowercase().contains(&needle)
                                        || p.name.to_lowercase().contains(&needle)
                                })
                                .collect();
                            ui.set_results_y(0.);
                            s.job(&ui, ReadKind::Search(query));
                            render(&ui, &s);
                        }
                    },
                );
            }
        });
    }
    {
        let s = state.clone();
        let w = weak.clone();
        ui.on_filter(move |category, sort| {
            if let Some(ui) = w.upgrade() {
                let mut s = s.borrow_mut();
                if s.view.page == 2 && s.view.category != category {
                    s.cancel_read(&ui);
                    s.view.tag_category = None;
                    s.view.packages = metadata::initial_packages(false);
                    s.view.heading = "Categories".into();
                    s.view.subtitle = "Choose a category to browse the editorial catalog or look up its WinGet tag.".into();
                }
                s.view.category = category;
                s.view.sort = sort;
                ui.set_results_y(0.);
                s.view.collection = ui.get_collection_index();
                render(&ui, &s);
            }
        });
    }
    {
        let s = state.clone();
        let w = weak.clone();
        ui.on_search_category(move || {
            if let Some(ui) = w.upgrade() {
                let mut s = s.borrow_mut();
                let Some(category) = CATEGORIES.get(s.view.category as usize).copied() else {
                    return;
                };
                if metadata::category_tag(category).is_none() {
                    return;
                }
                s.view.collection = 0;
                s.view.heading = format!("Explore {category}");
                s.view.subtitle = "Live packages classified by their WinGet manifest tags".into();
                s.view.tag_category = Some(category.into());
                ui.set_results_y(0.);
                s.job(&ui, ReadKind::Category(category.into()));
                render(&ui, &s);
            }
        });
    }
    {
        let s = state.clone();
        let w = weak.clone();
        ui.on_reflow(move || {
            let s2 = s.clone();
            let w = w.clone();
            s.borrow().resize_timer.start(
                slint::TimerMode::SingleShot,
                Duration::from_millis(60),
                move || {
                    if let Some(ui) = w.upgrade() {
                        render(&ui, &s2.borrow());
                    }
                },
            );
        });
    }
    {
        let s = state.clone();
        let w = weak.clone();
        ui.on_settings_changed(move || {
            if let Some(ui) = w.upgrade() {
                let mut s = s.borrow_mut();
                let previous_consent = s.settings.source_consent;
                apply_settings(&ui, &mut s, true);
                if previous_consent != s.settings.source_consent {
                    s.cache.clear();
                }
            }
        });
    }
    {
        let s = state.clone();
        let w = weak.clone();
        ui.on_clear_cache(move || {
            if let Some(ui) = w.upgrade() {
                s.borrow_mut().cache.clear();
                ui.set_status("Session metadata cache cleared".into());
            }
        });
    }
    {
        let w = weak.clone();
        ui.on_package_link(move |field| {
            if let Some(ui) = w.upgrade() {
                let detail = ui.get_detail();
                let url = if field == "website" {
                    detail.website
                } else {
                    detail.notes
                };
                if url.starts_with("https://") {
                    platform::open(url.as_str());
                }
            }
        });
    }
    let link_window = weak.clone();
    ui.on_link(move |key| match key.as_str() {
        "artwork" | "source" => {
            let Some(ui) = link_window.upgrade().filter(|ui| !ui.get_confirm_visible()) else { return };
            let (title, body) = if key == "artwork" {
                ("Artwork credits", include_str!("../catalog/artwork-credits.txt"))
            } else {
                ("Source availability", "SlimStore is licensed under GPL-3.0-only. Download the matching source archive from the same GitHub release as SlimStore.exe.\n\nThe source includes build instructions, license notices, and artwork credits.")
            };
            ui.set_confirm_title(title.into());
            ui.set_confirm_text(body.into());
            ui.set_confirm_agreement_required(false);
            ui.set_confirm_information(true);
            ui.set_confirm_visible(true);
        }
        "help" => platform::open("https://learn.microsoft.com/windows/package-manager/winget/"),
        "scoop-setup" => {
            if platform::information("Open the official Scoop setup guide?\n\nScoop installs for your Windows account and adds its shims to your user PATH. After setup, return here and choose Check for Scoop.", true) {
                platform::open("https://github.com/ScoopInstaller/Install#readme");
            }
        }
        "sources" => {
            platform::open("https://learn.microsoft.com/windows/package-manager/winget/source")
        }
        "license" => platform::open("https://slint.dev/terms-and-conditions#licenses"),
        _ => {}
    });
    {
        let s = state.clone();
        let w = weak.clone();
        ui.on_confirmed(move |yes| {
            let Some(ui) = w.upgrade().filter(|ui| ui.get_confirm_visible()) else {
                return;
            };
            if yes && ui.get_confirm_agreement_required() && !ui.get_package_consent() {
                return;
            }
            let mut s = s.borrow_mut();
            ui.set_confirm_visible(false);
            if ui.get_confirm_information() {
                ui.set_confirm_information(false);
                return;
            }
            let pending = s.pending.take();
            if !yes {
                if s.view.page == INSTALL_QUEUE_PAGE && matches!(pending, Some(Pending::Changes(_)))
                {
                    ui.set_status("Confirmation canceled. Selection kept.".into());
                }
                return;
            }
            match pending {
                Some(Pending::Changes(changes)) => {
                    let selection_keys = changes
                        .iter()
                        .map(|(p, verb)| (p.key(), *verb))
                        .collect::<Vec<_>>();
                    let a = ui.get_package_consent();
                    let source = s.settings.source_consent;
                    match s.manager.enqueue_group(changes, source, a) {
                        Ok(()) => {
                            for (key, verb) in selection_keys {
                                match verb {
                                    "install" => {
                                        s.install_selection.remove(&key);
                                    }
                                    "uninstall" => {
                                        s.uninstall_selection.remove(&key);
                                    }
                                    _ => {
                                        s.selected.remove(&key);
                                    }
                                }
                            }
                            s.view.page = 5;
                            ui.set_status("Operations queued. Open Downloads for progress.".into());
                            render(&ui, &s);
                        }
                        Err(error) => ui.set_status(error.into()),
                    }
                }
                Some(Pending::Close) => {
                    if s.manager.stop_after_active() {
                        ui.set_status(
                            "Waiting for the active installer. Close again when it finishes."
                                .into(),
                        );
                        render(&ui, &s);
                    } else {
                        let _ = slint::quit_event_loop();
                    }
                }
                None => {}
            }
        });
    }
    {
        let s = state.clone();
        let w = weak.clone();
        ui.window().on_close_requested(move || {
            use slint::CloseRequestResponse::{HideWindow, KeepWindowShown};
            let Some(ui) = w.upgrade() else { return HideWindow; };
            let mut s = s.borrow_mut();
            if s.manager.pending() == 0 {
                s.manager.stop_after_active();
                s.cancel_read(&ui);
                let _ = slint::quit_event_loop();
                return HideWindow;
            }
            if !s.manager.is_stopping() {
                s.pending = Some(Pending::Close);
                ui.set_confirm_title("Operations are still active".into());
                ui.set_confirm_text("Confirm to cancel queued operations. Let any active installer finish before closing SlimStore.".into());
                ui.set_confirm_agreement_required(false);
                ui.set_confirm_information(false);
                ui.set_confirm_visible(true);
            }
            KeepWindowShown
        });
    }

    ui.show()?;
    let chrome_timer = slint::Timer::default();
    {
        let w = weak.clone();
        // Winit creates the HWND as it enters the event loop.
        chrome_timer.start(
            slint::TimerMode::SingleShot,
            Duration::from_millis(1),
            move || {
                if let Some(ui) = w.upgrade() {
                    crate::chrome::install(ui.window(), ui.get_dark());
                }
            },
        );
    }
    let _smoke_timer = crate::smoke::install(&ui);
    slint::run_event_loop()?;
    ui.hide()?;
    state.borrow().manager.stop_after_active();
    state.borrow().generation.store(u64::MAX, Ordering::Relaxed);
    let _ = state.borrow().sender.send(Job {
        generation: u64::MAX,
        kind: ReadKind::Shutdown,
        source_consent: false,
    });
    let _ = read_handle.join();
    if let Some((sender, worker)) = state.borrow_mut().catalog.take() {
        drop(sender);
        let _ = worker.join();
    }
    STATE.with(|slot| *slot.borrow_mut() = None);
    UI_WEAK.with(|slot| *slot.borrow_mut() = None);
    Ok(())
}
