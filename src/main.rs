use std::{cell::RefCell, process::Command, sync::Arc};

use futures::StreamExt as _;
use rustc_hash::FxHashMap;
use zbus::{
    conn::Builder,
    proxy,
    zvariant::{OwnedObjectPath, OwnedValue, Value},
};

fn main() {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .build()
        .unwrap();
    rt.block_on(run());
}

struct Entry {
    path: OwnedObjectPath,
    mount_point: String,
    notification_id: u32,
}

async fn send_notification(
    proxy: &NotificationsProxy<'_>,
    summary: &str,
    body: &str,
    actions: &[&str],
    replaces_id: u32,
) -> Option<u32> {
    match proxy
        .notify(
            "udiskr",
            replaces_id,
            "",
            summary,
            body,
            actions,
            &Default::default(),
            30_000,
        )
        .await
    {
        Ok(x) => Some(x),
        Err(e) => {
            eprintln!("Notification failed: {e}");
            None
        }
    }
}

async fn run() {
    let system = Builder::system()
        .unwrap()
        .internal_executor(false)
        .build()
        .await
        .unwrap();
    let peer = PeerProxy::new(
        &system,
        "org.freedesktop.UDisks2",
        "/org/freedesktop/UDisks2/Manager",
    )
    .await
    .unwrap();
    peer.ping().await.unwrap();
    let manager = ManagerProxy::new(&system).await.unwrap();

    let mounted_devices: Arc<RefCell<Vec<Entry>>> = Arc::new(RefCell::new(Vec::new()));

    let session = Builder::session()
        .unwrap()
        .internal_executor(false)
        .build()
        .await
        .unwrap();
    
    let notification = NotificationsProxy::new(&session).await.unwrap();

    let notif_added = notification.clone();
    let notif_removed = notification.clone();
    let notif_invoked = notification.clone();

    let devices_added: Arc<RefCell<Vec<Entry>>> = Arc::clone(&mounted_devices);
    let devices_removed: Arc<RefCell<Vec<Entry>>> = Arc::clone(&mounted_devices);
    let devices_invoked: Arc<RefCell<Vec<Entry>>> = Arc::clone(&mounted_devices);

    tokio::join!(
        manager
            .receive_interfaces_added()
            .await
            .unwrap()
            .filter_map(|signal| async move { signal.args().map(|x| x.path).ok() })
            .filter(|obj_path| {
                let res = obj_path.starts_with("/org/freedesktop/UDisks2/block_devices");
                async move { res }
            })
            .filter_map(|obj_path| {
                let conn = &system;
                async move {
                    let fs = FilesystemProxy::new(conn, &obj_path).await.ok()?;
                    match fs.mount(&Default::default()).await {
                        Ok(mount_point) => Some((obj_path, mount_point)),
                        Err(ref e) => {
                            if let zbus::Error::MethodError(name, _, _) = e
                                && !name.starts_with("org.freedesktop.DBus")
                            {
                                eprintln!("{e}");
                            }
                            None
                        }
                    }
                }
            })
            .for_each(|(obj_path, mount_point)| {
                let msg = format!(
                    "Mounted /dev/{} at {}",
                    obj_path
                        .strip_prefix("/org/freedesktop/UDisks2/block_devices/")
                        .unwrap(),
                    mount_point
                );
                eprintln!("{}", &msg);
                
                let devices = Arc::clone(&devices_added);
                let notif = notif_added.clone(); 
                
                async move {
                    let id = send_notification(
                        &notif, 
                        "block device mounted", 
                        &msg, 
                        &["default", "open"], 
                        0
                    ).await;
                    
                    devices.borrow_mut().push(Entry {
                        path: obj_path,
                        mount_point,
                        notification_id: id.unwrap_or(0),
                    });
                }
            }),

        async move {
            let mut stream = manager.receive_interfaces_removed().await.unwrap();
            while let Some(x) = stream.next().await {
                let arg = x.args().unwrap();
                let mut vec = devices_removed.borrow_mut();
                if let Some(i) = vec.iter().position(|x| &x.path == &arg.path) {
                    let msg = format!(
                        "/dev/{} unmounted from {}",
                        arg.path
                            .strip_prefix("/org/freedesktop/UDisks2/block_devices/")
                            .unwrap(),
                        vec[i].mount_point
                    );
                    eprintln!("{}", &msg);
                    
                    // Use local clone
                    send_notification(
                        &notif_removed, 
                        "block device unmounted", 
                        &msg, 
                        &[], 
                        vec[i].notification_id
                    ).await;
                    
                    vec.swap_remove(i);
                }
            }
        },

        async move {
            let mut stream = notif_invoked.receive_action_invoked().await.unwrap();
            while let Some(x) = stream.next().await {
                let arg = x.args().unwrap();
                if let Some(Entry { mount_point, .. }) = devices_invoked
                    .borrow()
                    .iter()
                    .find(|x| x.notification_id == arg.id)
                {
                    Command::new("xdg-open")
                        .arg(mount_point)
                        .spawn()
                        .err()
                        .map(|e| eprintln!("Failed to open dir: {e}"));
                }
            }
        },
    );
}

#[proxy(
    default_service = "org.freedesktop.UDisks2",
    default_path = "/org/freedesktop/UDisks2",
    interface = "org.freedesktop.DBus.ObjectManager"
)]
trait Manager {
    #[zbus(signal)]
    fn interfaces_added(
        &self,
        path: OwnedObjectPath,
        interfaces_and_properties: FxHashMap<String, FxHashMap<String, OwnedValue>>,
    );
    #[zbus(signal)]
    fn interfaces_removed(&self, path: OwnedObjectPath, interfaces: Vec<String>);
}

#[proxy(
    default_service = "org.freedesktop.UDisks2",
    interface = "org.freedesktop.UDisks2.Filesystem"
)]
trait Filesystem {
    fn mount(&self, options: &FxHashMap<&str, Value<'_>>) -> zbus::Result<String>;
}

#[proxy(
    interface = "org.freedesktop.Notifications",
    default_service = "org.freedesktop.Notifications",
    default_path = "/org/freedesktop/Notifications"
)]
trait Notifications {
    fn notify(
        &self,
        app_name: &str,
        replaces_id: u32,
        app_icon: &str,
        summary: &str,
        body: &str,
        actions: &[&str],
        hints: &FxHashMap<&str, Value<'_>>,
        expire_timeout: i32,
    ) -> zbus::Result<u32>;
    #[zbus(signal)]
    fn action_invoked(&self, id: u32, action_key: &str);
}

#[proxy(interface = "org.freedesktop.DBus.Peer")]
trait Peer {
    fn ping(&self) -> zbus::Result<()>;
}
