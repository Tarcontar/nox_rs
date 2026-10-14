; Windows installer for Nox (Inno Setup 6.3 or newer), made from the
; package tree of `cargo xtask dist`. CI builds it like this:
;
;   iscc /DVersion=0.1.0 /DArch=x64 /DSource=<repo>\target\dist\nox-0.1.0-windows-x64
;        /DOutput=<repo>\target\dist packaging\windows\nox.iss
;
; Arch is x64 or arm64. Installs for the current user by default (no
; administrator rights); the first page offers all users instead.

#ifndef Version
  #error Define Version, Arch, Source and Output (see the top of this file)
#endif

[Setup]
; Identifies Nox for updates and uninstalling: never change it.
AppId={{5D993737-0796-4D35-A84B-4AD67BE20BB7}
AppName=Nox
AppVersion={#Version}
AppVerName=Nox {#Version}
AppPublisher=Stephan Vedder
AppPublisherURL=https://github.com/feliwir/nox_rs
AppSupportURL=https://github.com/feliwir/nox_rs/issues
DefaultDirName={autopf}\Nox
DefaultGroupName=Nox
DisableProgramGroupPage=yes
PrivilegesRequired=lowest
PrivilegesRequiredOverridesAllowed=dialog
#if Arch == "arm64"
ArchitecturesAllowed=arm64
ArchitecturesInstallIn64BitMode=arm64
#else
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
#endif
OutputDir={#Output}
OutputBaseFilename=nox-{#Version}-windows-{#Arch}-setup
SetupIconFile=..\..\data\icons\windows\io.github.feliwir.Nox.ico
UninstallDisplayIcon={app}\bin\nox.exe
LicenseFile={#Source}\LICENSE.txt
Compression=lzma2/max
SolidCompression=yes
WizardStyle=modern
ChangesAssociations=yes

[Languages]
Name: "en"; MessagesFile: "compiler:Default.isl"
Name: "de"; MessagesFile: "compiler:Languages\German.isl"

[CustomMessages]
en.AssociateOtio=Open OpenTimelineIO projects (.otio) with Nox
de.AssociateOtio=OpenTimelineIO-Projekte (.otio) mit Nox öffnen
en.OtioProject=OpenTimelineIO project
de.OtioProject=OpenTimelineIO-Projekt

[Tasks]
Name: "desktopicon"; Description: "{cm:CreateDesktopIcon}"; GroupDescription: "{cm:AdditionalIcons}"; Flags: unchecked
Name: "otio"; Description: "{cm:AssociateOtio}"

[Files]
Source: "{#Source}\*"; DestDir: "{app}"; Flags: ignoreversion recursesubdirs createallsubdirs

[Icons]
Name: "{autoprograms}\Nox"; Filename: "{app}\bin\nox.exe"
Name: "{autodesktop}\Nox"; Filename: "{app}\bin\nox.exe"; Tasks: desktopicon

[Registry]
Root: HKA; Subkey: "Software\Classes\.otio\OpenWithProgids"; ValueType: string; ValueName: "Nox.Project"; ValueData: ""; Flags: uninsdeletevalue; Tasks: otio
Root: HKA; Subkey: "Software\Classes\Nox.Project"; ValueType: string; ValueName: ""; ValueData: "{cm:OtioProject}"; Flags: uninsdeletekey; Tasks: otio
Root: HKA; Subkey: "Software\Classes\Nox.Project\DefaultIcon"; ValueType: string; ValueName: ""; ValueData: "{app}\bin\nox.exe,0"; Tasks: otio
Root: HKA; Subkey: "Software\Classes\Nox.Project\shell\open\command"; ValueType: string; ValueName: ""; ValueData: """{app}\bin\nox.exe"" ""%1"""; Tasks: otio

[Run]
Filename: "{app}\bin\nox.exe"; Description: "{cm:LaunchProgram,Nox}"; Flags: nowait postinstall skipifsilent
