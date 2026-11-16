; Script de Inno Setup para Ghost Browser.
; Compilar con: iscc installer\ghost.iss  (desde la raíz del repo)
; El binario release debe existir en target\release\ghost.exe.

#define MyAppName "Ghost Browser"
#define MyAppVersion "0.1.0"
#define MyAppPublisher "sarch-codelab"
#define MyAppExeName "ghost.exe"

[Setup]
AppId={{7E4A9C21-3B5D-4F8E-9A6C-2D1E0B8F4A37}
AppName={#MyAppName}
AppVersion={#MyAppVersion}
AppVerName={#MyAppName} {#MyAppVersion}
AppPublisher={#MyAppPublisher}
DefaultDirName={localappdata}\Programs\Ghost
DefaultGroupName=Ghost Browser
DisableProgramGroupPage=yes
OutputDir={#SourcePath}\output
OutputBaseFilename=GhostSetup
Compression=lzma2/ultra64
SolidCompression=yes
WizardStyle=modern
PrivilegesRequired=lowest
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
UninstallDisplayIcon={app}\{#MyAppExeName}
SetupIconFile={#SourcePath}\..\ghost.ico

[Languages]
Name: "spanish"; MessagesFile: "compiler:Languages\Spanish.isl"

[Tasks]
Name: "desktopicon"; Description: "{cm:CreateDesktopIcon}"; GroupDescription: "{cm:AdditionalIcons}"; Flags: unchecked

[Files]
Source: "{#SourcePath}\..\target\release\{#MyAppExeName}"; DestDir: "{app}"; Flags: ignoreversion

[Icons]
Name: "{group}\{#MyAppName}"; Filename: "{app}\{#MyAppExeName}"; IconFilename: "{app}\{#MyAppExeName}"
Name: "{group}\{cm:UninstallProgram,{#MyAppName}}"; Filename: "{uninstallexe}"
Name: "{autodesktop}\{#MyAppName}"; Filename: "{app}\{#MyAppExeName}"; Tasks: desktopicon; IconFilename: "{app}\{#MyAppExeName}"

[Code]
var
  WebView2ResultCode: Integer;

function WebView2NotInstalled: Boolean;
begin
  // Reg.exe devuelve 0 si la clave existe (WebView2 instalado).
  Exec('reg', 'query "HKLM\SOFTWARE\WOW6432Node\Microsoft\EdgeUpdate\Clients\{F3017226-FE2A-4295-8BDF-6E3C277E087F}" /v pv', '', SW_HIDE, ewWaitUntilTerminated, WebView2ResultCode);
  Result := WebView2ResultCode <> 0;
end;

procedure CurStepChanged(CurStep: TSetupStep);
begin
  if CurStep = ssPostInstall then
    if WebView2NotInstalled then
      MsgBox('Ghost Browser necesita Microsoft Edge WebView2 Runtime para las páginas complejas. Parece que no está instalado: descárgalo gratis en https://developer.microsoft.com/microsoft-edge/webview2/', mbInformation, MB_OK);
end;

[UninstallDelete]
; Limpieza de los perfiles de usuario que Ghost crea.
Type: filesandordirs; Name: "{userappdata}\Ghost"
Type: filesandordirs; Name: "{localappdata}\Ghost"
