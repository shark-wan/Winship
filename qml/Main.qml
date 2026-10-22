import QtQuick
import QtQuick.Controls
import QtQuick.Layouts
import org.kde.kirigami as Kirigami
import org.winship.app 1.0

Kirigami.ApplicationWindow {
    id: root
    title: "Winship"
    width: 1180
    height: 760
    minimumWidth: 960
    minimumHeight: 640
    visible: true

    pageStack.initialPage: Kirigami.ScrollablePage {
        title: "Winship"

        ColumnLayout {
            spacing: Kirigami.Units.largeSpacing
            width: parent.width

            Kirigami.Heading {
                text: "Winship 正在启动"
                level: 1
            }

            Label {
                text: "cxx-qt + Kirigami 骨架已加载"
                opacity: 0.7
            }

            Kirigami.Card {
                width: parent.width
                contentItem: ColumnLayout {
                    Label {
                        text: "后端状态"
                        font.bold: true
                    }
                    Label {
                        text: Winship.stateJson
                        wrapMode: Text.WrapAnywhere
                        font.family: "monospace"
                        Layout.fillWidth: true
                    }
                }
            }
        }

        Component.onCompleted: Winship.refresh()
    }
}
