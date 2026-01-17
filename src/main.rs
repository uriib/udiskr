#![feature(future_join)]
use std::{cell::RefCell, future::join, process::Command};

use futures::{StreamExt, executor::block_on};
use rustc_hash::FxHashMap;
use zbus::{
    Connection, proxy,
    zvariant::{OwnedObjectPath, OwnedValue},
};

fn main() {
    block_on(run());
}

struct Entry {
    path: OwnedObjectPath,
    mount_point: String,
    notification_id: u32,
}

async fn run() {
    let system = Connection::system().await.unwrap();
    let peer = PeerProxy::new(
        &system,
        "org.freedesktop.UDisks2",
        "/org/freedesktop/UDisks2/Manager",
    )
    .await
    .unwrap();
    peer.ping().await.unwrap();
    let manager = ManagerProxy::new(&system).await.unwrap();
    let mounted_devices: RefCell<Vec<Entry>> = Default::default();

    let session = Connection::session().await.unwrap();
    let notification = NotificationsProxy::new(&session).await.unwrap();
    let notify = async |summary, body: String, actions, replaces_id: u32| match notification
        .notify(
            "udiskr",
            replaces_id,
            "",
            summary,
            body.as_str(),
            actions,
            &Default::default(),
            30_000,
        )
        .await
    {
        Ok(x) => Some(x),
        Err(e) => {
            eprintln!("Failed to send notification: {}", e);
            None
        }
    };

    join!(
        manager
            .receive_interfaces_added()
            .await
            .unwrap()
            .filter_map(async |signal| signal.args().map(|x| x.path).ok())
            .filter(|obj_path| {
                let res = obj_path.starts_with("/org/freedesktop/UDisks2/block_devices");
                async move { res }
            })
            .filter_map(|obj_path| {
                let conn = &system;
                async move {
                    let fs = FilesystemProxy::new(conn, &obj_path).await.ok()?;
                    match fs.mount(Default::default()).await {
                        Ok(mount_point) => Some((obj_path, mount_point)),
                        Err(ref e) => {
                            if let zbus::Error::MethodError(name, _, _) = e
                                && !name.starts_with("org.freedesktop.DBus")
                            {
                                eprintln!("Failed to mount device: {}", e);
                            }
                            None
                        }
                    }
                }
            })
            .for_each(async |(obj_path, mount_point)| {
                let msg = format!(
                    "Mounted /dev/{} at {}",
                    obj_path
                        .strip_prefix("/org/freedesktop/UDisks2/block_devices/")
                        .unwrap(),
                    mount_point
                );
                eprintln!("{}", &msg);
                let id = notify("block device mounted", msg, &["default", "open"], 0).await;
                mounted_devices.borrow_mut().push(Entry {
                    path: obj_path,
                    mount_point,
                    notification_id: id.unwrap_or(0),
                });
            }),
        async {
            let mut stream = manager.receive_interfaces_removed().await.unwrap();
            loop {
                let x = stream.next().await.unwrap();
                let arg = x.args().unwrap();

                let mut vec = mounted_devices.borrow_mut();
                if let Some(i) = vec.iter().position(|x| &x.path == &arg.path) {
                    let msg = format!(
                        "/dev/{} unmounted from {}",
                        arg.path
                            .strip_prefix("/org/freedesktop/UDisks2/block_devices/")
                            .unwrap(),
                        vec[i].mount_point
                    );
                    eprintln!("{}", &msg);
                    notify("block device unmounted", msg, &[], vec[i].notification_id).await;
                    vec.swap_remove(i);
                }
            }
        },
        async {
            let mut stream = notification.receive_action_invoked().await.unwrap();
            loop {
                let x = stream.next().await.unwrap();
                let arg = x.args().unwrap();
                if let Some(Entry { mount_point, .. }) = mounted_devices
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
    )
    .await;
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
    fn mount(&self, options: FxHashMap<String, OwnedValue>) -> zbus::Result<String>;
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
        hints: &FxHashMap<String, OwnedValue>,
        expire_timeout: i32,
    ) -> zbus::Result<u32>;
    #[zbus(signal)]
    fn action_invoked(&self, id: u32, action_key: &str);
}

#[proxy(interface = "org.freedesktop.DBus.Peer")]
trait Peer {
    fn ping(&self) -> zbus::Result<()>;
}
