//! Android `WifiManager.MulticastLock` via JNI.
//!
//! Android drops inbound multicast packets unless an app holds a MulticastLock,
//! which would silently break mDNS discovery. We acquire one at startup and hold
//! it for the app's lifetime (released on drop).

use anyhow::{Context, Result};
use jni::objects::{GlobalRef, JObject, JValue};
use jni::JavaVM;
use winit::platform::android::activity::AndroidApp;

/// Holds an acquired multicast lock; releases it on drop.
pub struct MulticastLock {
    vm: JavaVM,
    lock: GlobalRef,
}

impl MulticastLock {
    /// Acquire `WifiManager.createMulticastLock(...).acquire()` for this app.
    pub fn acquire(app: &AndroidApp) -> Result<MulticastLock> {
        let vm = unsafe { JavaVM::from_raw(app.vm_as_ptr().cast()) }.context("JavaVM::from_raw")?;
        let activity_ptr = app.activity_as_ptr();
        let mut env = vm.attach_current_thread().context("attach JNI thread")?;
        let activity = unsafe { JObject::from_raw(activity_ptr.cast()) };

        // WIFI_SERVICE name.
        let wifi_service = env
            .get_static_field("android/content/Context", "WIFI_SERVICE", "Ljava/lang/String;")
            .context("Context.WIFI_SERVICE")?
            .l()?;
        // WifiManager wm = (WifiManager) ctx.getSystemService(WIFI_SERVICE);
        let wifi_manager = env
            .call_method(
                &activity,
                "getSystemService",
                "(Ljava/lang/String;)Ljava/lang/Object;",
                &[JValue::Object(&wifi_service)],
            )
            .context("getSystemService(WIFI_SERVICE)")?
            .l()?;
        // MulticastLock lock = wm.createMulticastLock("newfoundsync");
        let tag = env.new_string("newfoundsync")?;
        let lock = env
            .call_method(
                &wifi_manager,
                "createMulticastLock",
                "(Ljava/lang/String;)Landroid/net/wifi/WifiManager$MulticastLock;",
                &[JValue::Object(&tag)],
            )
            .context("createMulticastLock")?
            .l()?;
        // lock.setReferenceCounted(false); lock.acquire();
        env.call_method(&lock, "setReferenceCounted", "(Z)V", &[JValue::Bool(0)])?;
        env.call_method(&lock, "acquire", "()V", &[])
            .context("MulticastLock.acquire")?;

        let lock = env.new_global_ref(&lock)?;
        drop(env); // release the AttachGuard's borrow of `vm` before moving it
        log::info!("mDNS multicast lock acquired");
        Ok(MulticastLock { vm, lock })
    }
}

impl Drop for MulticastLock {
    fn drop(&mut self) {
        if let Ok(mut env) = self.vm.attach_current_thread() {
            let _ = env.call_method(self.lock.as_obj(), "release", "()V", &[]);
            log::info!("mDNS multicast lock released");
        }
    }
}
