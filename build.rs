use cxx_qt_build::{CxxQtBuilder, QmlModule};

fn main() {
    CxxQtBuilder::new_qml_module(QmlModule::new("org.winship.app").qml_files(["qml/Main.qml"]))
        .qt_module("Qml")
        .files(["src/bridge.rs"])
        .build();
}
