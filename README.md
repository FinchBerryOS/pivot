# pivot — FinchBerryOS initramfs
**The first process. The last line of defense.**

`pivot` ist das PID 1 Init-Binary, das fest im FinchBerryOS-Initramfs integriert ist. Es wurde vollständig in **Rust** geschrieben und ist dafür verantwortlich, das finale Root-Dateisystem aus einem schreibgeschützten **SquashFS-Image** und den persistenten Nutzerdaten zusammenzusetzen, bevor es die Kontrolle an `syscored` übergibt.

---

## Übersicht
Moderne Linux-Systeme booten in eine minimale RAM-basierte Umgebung (Initramfs), bevor sie zum echten Root-Dateisystem wechseln. `pivot` ist das Herzstück dieser Umgebung.

Es führt die gesamte Boot-Sequenz in einem einzigen, deterministischen Durchlauf aus:

1. **Kernel lädt Initramfs**
   └── **pivot (PID 1)**
       ├── Mountet virtuelle Dateisysteme (`/proc`, `/sys`, `/dev`)
       ├── Liest `pivot.config` (TOML)
       ├── Lokalisiert Partitionen via **PARTUUID**
       ├── Mountet die **System Partition (SP)** nach `/mnt/system`
       ├── [Optional] Wechselt in den **RAM Update Mode**
       ├── Erstellt das Root-Dateisystem-"Sandwich" unter `/system/rootfs`
       ├── Verschiebt VFS-Mounts in das neue Root
       ├── `pivot_root(2)`
       └── `exec /usr/libexec/syscored` → Neuer PID 1

---

## Boot-Modi

### Normal Boot
Der Standardpfad. `pivot` mountet das aktive A/B-Slot-Image (SquashFS), verbindet es mit den persistenten Ordnern der SP (Users, Library, private) und "pivoted" in das Resultat.

### Live / Installer Mode
Wenn `pivot.config` den Wert `mode = "live"` enthält, wird der normale Bootpfad übersprungen. Dies ist für Installationsmedien vorgesehen, um das System direkt aus dem RAM oder vom USB-Stick zu installieren.

### RAM Update Mode
Wird durch einen "Double-Flag"-Check ausgelöst:
1. `/mnt/system/private/system/StartUpdateInstaller` (Trigger-Datei)
2. `/mnt/system/var/update/sys_update.fbuimg` (Update-Payload)

Wenn beide vorhanden sind, kopiert `pivot` den `updateinstaller` in ein `tmpfs` im RAM, hängt die System-Partition aus (um dem Flasher direkten Block-Zugriff zu geben) und führt den Updater vollständig im RAM aus. Dies ermöglicht atomare, sichere In-Place-System-Updates ohne Beeinträchtigung des laufenden Systems.

---

## Dateisystem-Architektur
`pivot` konstruiert ein geschichtetes Dateisystem-Sandwich unter `/system/rootfs`:

| Mount-Punkt | Quelle | Modus | Beschreibung |
| :--- | :--- | :--- | :--- |
| `/System`, `/usr`, `/bin`, `/sbin` | SquashFS Image | **RO** | Unveränderlicher System-Kern |
| `/Applications/<CoreApp.app>` | SquashFS Image | **RO** | System-Apps (Finder, Terminal, etc.) |
| `/Applications` | System Partition | **RW** | Ort für Nutzer-installierte Programme |
| `/Users` | System Partition | **RW** | Home-Verzeichnisse der Benutzer |
| `/Library` | System Partition | **RW** | Persistente Anwendungsdaten & Frameworks |
| `/private` | System Partition | **RW** | Konfigurationen (`/etc`) und Daten (`/var`) |
| `/Volumes` | System Partition | **RW** | **Zentraler Einhängepunkt für externe Medien** |
| `/run` | `tmpfs` | **RW** | Flüchtige Daten (PIDs, Sockets) |

### Der `/Volumes` Ordner
Im Gegensatz zu anderen Systemordnern enthält `/Volumes` keine permanenten Daten. Er dient als dynamischer Einhängepunkt. Der Ankerpunkt liegt physisch auf der **System Partition**, damit das System dort jederzeit Unterordner für externe Laufwerke (z. B. USB-Sticks) erstellen kann, ohne das schreibgeschützte Haupt-Image verändern zu müssen.

### Der hybride `/Applications` Ordner
Nutzer-Apps liegen beschreibbar auf der SP. System-Apps aus dem SquashFS-Image werden einzeln als Read-Only Bind-Mounts in diesen Ordner "gepinnt", um sie vor Manipulation zu schützen.

---

## Konfiguration
`pivot` erwartet eine `/pivot.config` im Root des Initramfs.

```toml
[system]
mode = "installed"   # "installed" oder "live"
active_slot = "A"    # Aktueller Slot: "A" oder "B"

[hardware]
boot_partition_uuid   = "XXXX-XXXX"
system_partition_uuid = "xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx"

[images]
slot_a = "base_system_a.img"
slot_b = "base_system_b.img"