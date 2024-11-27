use std::any::{self, Any, TypeId};
use std::collections::HashMap;
use std::sync::Arc;
use self::errors::*;

/// It is a kind of DI Container shared within the [`Context`](crate::Context) that each Process has.
/// 
/// Behaves roughly the same as [`Extension` of `http` crate](https://docs.rs/http/1.1.0/http/struct.Extensions.html).
/// 
/// This container uses **dynamic dispatching**, 
/// so processes such as extracting contents are resolved at runtime.
pub struct Extensions {
    ext: Arc<HashMap<TypeId, Box<dyn Any + Sync + Send>>>
}

impl Clone for Extensions {
    fn clone(&self) -> Self {
        Self { ext: Arc::clone(&self.ext) }
    }
}

impl Extensions {
    pub fn builder() -> Installer {
        Installer::default()
    }

    pub fn get<T>(&self) -> Result<&T, Missing>
    where T: Clone + Sync + Send + 'static
    {
        self.ext.get(&TypeId::of::<T>())
            .and_then(|ext| ext.downcast_ref::<T>())
            .ok_or(Missing(any::type_name::<T>()))
    }
}

#[derive(Default)]
pub struct Installer {
    ext: HashMap<TypeId, Box<dyn Any + Sync + Send>>
}

impl Installer {
    pub fn install<T>(&mut self, ext: T) -> Result<&mut Self, AlreadyInstalled>
    where T: Clone + Sync + Send + 'static
    {
        let id = TypeId::of::<T>();
        if self.ext.contains_key(&id) {
            return Err(AlreadyInstalled(any::type_name::<T>()));
        }
        self.ext.insert(id, Box::new(ext));
        Ok(self)
    }
    
    pub fn build(self) -> Extensions {
        Extensions { ext: Arc::new(self.ext) }
    }
}


pub mod errors {
    #[derive(Debug, thiserror::Error)]
    #[error("`extension = {0}` was already installed.")]
    pub struct AlreadyInstalled(pub &'static str);
    
    #[derive(Debug, thiserror::Error)]
    #[error("`extension = {0}` is not install.")]
    pub struct Missing(pub &'static str);
}


#[cfg(test)]
mod tests {
    use crate::extension::Extensions;

    #[derive(Debug, Clone, Eq, PartialEq)]
    struct TestModule;
    
    #[test]
    fn module_install_and_get() {
        let mut ext = Extensions::builder();
        ext.install(TestModule).unwrap();
        let ext = ext.build();
        let module = ext.get::<TestModule>().unwrap();
        assert_eq!(&TestModule, module);
    }
    
    #[test]
    #[should_panic]
    fn module_install_failure_on_module_already_installed() {
        let mut ext = Extensions::builder();
        ext.install(TestModule).unwrap();
        ext.install(TestModule).unwrap(); // PANIC
    }
    
    #[test]
    #[should_panic]
    fn module_get_failure_on_missing_module() {
        let ext = Extensions::builder().build();
        let _panic = ext.get::<TestModule>().unwrap();
    }
}