; pTerminal per-user Windows installer (Inno Setup 6).
; Built by .github/workflows/release.yml as:
;   iscc /DAppVersion=x.y.z installer\pterminal.iss
; Design doc: docs/superpowers/specs/2026-08-12-auto-update-installer-design.md

#ifndef AppVersion
  #define AppVersion "0.0.0"  ; dev builds only — CI always passes the real one
#endif

[Setup]
; Fixed AppId so every release installs over the previous one (update-in-place).
AppId={{8B1F1E7A-4C0D-4E9E-9D3B-7C2A51B0F9D4}
AppName=pTerminal
AppVersion={#AppVersion}
; Per-user: no UAC, and the one-click /SILENT update needs no elevation.
PrivilegesRequired=lowest
DefaultDirName={userpf}\pTerminal
DisableProgramGroupPage=yes
; The user-PATH append below; tells Windows to broadcast the change.
ChangesEnvironment=yes
OutputDir=Output
OutputBaseFilename=pTerminal-setup
Compression=lzma2
SolidCompression=yes
UninstallDisplayIcon={app}\pterminal.exe

[Files]
Source: "..\target\release\pterminal.exe"; DestDir: "{app}"; Flags: ignoreversion
; hooks locate pterm_hook.exe NEXT TO pterminal.exe (src/hooks.rs) — both ship.
Source: "..\target\release\pterm_hook.exe"; DestDir: "{app}"; Flags: ignoreversion

[Icons]
Name: "{userprograms}\pTerminal"; Filename: "{app}\pterminal.exe"

[Run]
; postinstall WITHOUT skipifsilent: the auto-updater runs this installer
; /SILENT, and this entry is what relaunches pTerminal afterward. In an
; interactive install it's the "Launch pTerminal" checkbox on the finish page.
Filename: "{app}\pterminal.exe"; Description: "Launch pTerminal"; Flags: nowait postinstall

[Registry]
; Append {app} to the USER Path so `pterminal resume --id <sid>` works from
; any shell. Guarded so repeat installs don't stack duplicates.
; ponytail: uninstall leaves the PATH entry behind (harmless, points nowhere);
; strip-on-uninstall code isn't worth its bug surface.
Root: HKCU; Subkey: "Environment"; ValueType: expandsz; ValueName: "Path"; ValueData: "{olddata};{app}"; Check: NeedsAddPath(ExpandConstant('{app}'))

[Code]
function NeedsAddPath(Param: string): boolean;
var
  OrigPath: string;
begin
  if not RegQueryStringValue(HKEY_CURRENT_USER, 'Environment', 'Path', OrigPath) then
  begin
    Result := True;
    exit;
  end;
  { look for the dir bounded by semicolons (also match at either end) }
  Result := Pos(';' + Uppercase(Param) + ';', ';' + Uppercase(OrigPath) + ';') = 0;
end;
