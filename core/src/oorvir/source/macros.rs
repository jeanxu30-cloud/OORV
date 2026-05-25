#[macro_export]
macro_rules! bind_analysis_delegate {
    // Match an impl block listing zero or more method signatures.
    // Each signature has &self plus any number of typed arguments.
    (
        impl $trait:ident for $host:ident via $field:ident ( $msg:literal ) {
            $( fn $method:ident ( &self $(, $arg:ident : $ty:ty)* ) -> $ret:ty ; )*
        }
    ) => {
        impl $trait for $host {
            $(
                fn $method(&self $(, $arg: $ty)*) -> $ret {
                    // Reach into the Option field, panicking with a descriptive
                    // message when the required analysis pass has not been run.
                    self.$field
                        .as_ref()
                        .expect($msg)
                        .$method($($arg),*)
                }
            )*
        }
    };
}
