use std::cell::UnsafeCell;
use std::fmt::{Debug, Display};
use std::ops::{Deref, DerefMut};
use std::sync::atomic::{AtomicU64, Ordering};

pub struct OptLock<T: ?Sized> {
    version: AtomicU64,
    cell: UnsafeCell<T>
}

#[derive(Debug, thiserror::Error)]
pub enum ErrorKind {
    #[error("Blocked by Writer.")]
    Blocked,
    #[error("Version information does not match")]
    MismatchedVersion
}

impl<T> OptLock<T> {
    pub fn new(value: T) -> Self {
        OptLock {
            version: AtomicU64::new(0),
            cell: UnsafeCell::new(value)
        }
    }
}

unsafe impl<T: ?Sized + Send> Send for OptLock<T> {}
unsafe impl<T: ?Sized + Send + Sync> Sync for OptLock<T> {}

impl<T: ?Sized> OptLock<T> {
    async fn lock(&self) -> Result<u64, ErrorKind> {
        let version = async {
            let version = self.version.load(Ordering::Acquire);
            if version & 0b1 != 0 {
                return Err(ErrorKind::Blocked)
            }
            Ok(version)
        };
        version.await
    }

    pub async fn read(&self) -> Result<ReadGuard<T>, ErrorKind> {
        let version = self.lock().await?;
        Ok(ReadGuard { version, lock: self })
    }

    pub async fn write(&self) -> Result<WriteGuard<T>, ErrorKind> {
        let version = self.lock().await?;
        match self.version.compare_exchange(version, version + 0b1, Ordering::Acquire, Ordering::Acquire) { 
            Ok(_) => Ok(WriteGuard { lock: self }),
            Err(_) => Err(ErrorKind::MismatchedVersion)
        }
    }
}

pub struct ReadGuard<'a, T: ?Sized + 'a> {
    version: u64,
    lock: &'a OptLock<T>
}

impl<T: ?Sized> ReadGuard<'_, T> {
    pub async fn sync(self) -> Result<(), ErrorKind> {
        if self.version == self.lock.lock().await? {
            Ok(())
        } else {
            Err(ErrorKind::MismatchedVersion)
        }
    }
}

impl<T: ?Sized> Deref for ReadGuard<'_, T> {
    type Target = T;
    
    fn deref(&self) -> &T {
        unsafe { &*self.lock.cell.get() }
    }
}

impl<T: Debug> Debug for ReadGuard<'_, T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ReadGuard")
            .field("version", &(self.version >> 1))
            .field("data", self.deref())
            .finish()
    }
}

impl<T: Display> Display for ReadGuard<'_, T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_fmt(format_args!("ReadGuard(v: {}): {}", self.version >> 1, self.deref()))
    }
}


pub struct WriteGuard<'a, T: ?Sized + 'a> {
    lock: &'a OptLock<T>
}

impl<T: ?Sized> Deref for WriteGuard<'_, T> {
    type Target = T;
    
    fn deref(&self) -> &T {
        unsafe { &*self.lock.cell.get() }
    }
}

impl<T: ?Sized> DerefMut for WriteGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        unsafe { &mut *self.lock.cell.get() }
    }
}

impl<T: Debug> Debug for WriteGuard<'_, T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WriteGuard")
            .field("version", &(self.lock.version.load(Ordering::Relaxed) >> 1))
            .field("data", self.deref())
            .finish()
    }
}

impl<T: Display> Display for WriteGuard<'_, T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_fmt(format_args!("WriteGuard(v: {}): {}", self.lock.version.load(Ordering::Relaxed) >> 1, self.deref()))
    }
}

impl<T: ?Sized> Drop for WriteGuard<'_, T> {
    fn drop(&mut self) {
        self.lock.version.fetch_add(0b1, Ordering::Release);
    }
}

unsafe impl<T: ?Sized + Sync> Sync for WriteGuard<'_, T> {}


#[cfg(test)]
mod test {
    use super::*;
    use std::sync::Arc;
    use tokio::time::sleep;
    use tracing::level_filters::LevelFilter;
    use tracing_subscriber::EnvFilter;
    
    #[tokio::test]
    async fn threading() {
        std::env::set_var("RUST_LOG", "spectroscopy=trace,test_agent=trace");
        tracing_subscriber::fmt()
            .with_env_filter(EnvFilter::from_default_env())
            .with_max_level(LevelFilter::TRACE)
            .init();
        
        
        let val = Arc::new(OptLock::new(0));
        
        let mut tasks = vec![];
        for i in 0..10 {
            let a = Arc::clone(&val);
            let task = tokio::spawn(async move {
                loop {
                    sleep(std::time::Duration::from_secs((i % 2) * 5)).await;
                    match a.write().await {
                        Ok(mut guard) => {
                            *guard += 1;
                            tracing::debug!("[{:?}] {}", std::thread::current().id(), guard);
                            break;
                        }
                        Err(e) => {
                            tracing::debug!("Writer Error: {:?}", e);
                            continue;
                        }
                    }
                }
            });
            tasks.push(task);
            
            let b = Arc::clone(&val);
            let task = tokio::spawn(async move {
                loop {
                    match b.read().await {
                        Ok(guard) => {
                            sleep(std::time::Duration::from_secs(i % 2) * 5).await;
                            tracing::debug!("Read: {}", guard);
                            match guard.sync().await {
                                Ok(_) => {
                                    break;
                                },
                                Err(e) => {
                                    tracing::debug!("Reader Xact Error: {:?}", e);
                                    continue;
                                }
                            }
                        }
                        Err(e) => {
                            tracing::debug!("Reader Error: {:?}", e);
                            continue;
                        }
                    }
                }
            });
            tasks.push(task);
        }
        
        let _ = futures::future::join_all(tasks).await;
    }
}