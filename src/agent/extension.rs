use std::any::{Any, TypeId};
use std::collections::HashMap;
use std::error::Error;
use std::fmt::{Display, Formatter};

#[derive(Default)]
pub struct Extensions {
    ext: HashMap<TypeId, Box<dyn Any + Sync + Send>>
}

impl Extensions {
    pub fn install<T>(&mut self, ext: T) -> &mut Self
    where T: Clone + Sync + Send + 'static
    {
        self.ext.insert(TypeId::of::<T>(), Box::new(ext));
        self
    }

    pub fn get<T>(&self) -> Option<&T>
    where T: Clone + Sync + Send + 'static
    {
        self.ext.get(&TypeId::of::<T>())
            .and_then(|boxed| boxed.downcast_ref())
    }
}


#[derive(Debug)]
pub struct ExtensionMissingError {
    pub module: &'static str,
}

impl Display for ExtensionMissingError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "extension = `{}` does not installed in ActorSystem.", self.module)
    }
}

impl Error for ExtensionMissingError {}
