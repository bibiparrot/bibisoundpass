#ifndef AppVersion
  #define AppVersion "0.1.4"
#endif
#ifndef SourceDir
  #define SourceDir "."
#endif
#ifndef OutputDir
  #define OutputDir "."
#endif

[Setup]
AppId={{0C67A91B-B9F9-4692-8A8D-A02C7F1BDA3E}
AppName=SoundPass
AppVersion={#AppVersion}
AppPublisher=bibiparrot
AppPublisherURL=https://github.com/bibiparrot/bibisoundpass
DefaultDirName={autopf}\SoundPass
DisableProgramGroupPage=yes
PrivilegesRequired=admin
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
OutputDir={#OutputDir}
OutputBaseFilename=soundpass-{#AppVersion}-windows-x86_64_setup
Compression=lzma2
SolidCompression=yes
ChangesEnvironment=yes
UninstallDisplayIcon={app}\soundpass-send.exe

[Files]
Source: "{#SourceDir}\soundpass-send.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#SourceDir}\soundpass-recv.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#SourceDir}\README.md"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#SourceDir}\LICENSE"; DestDir: "{app}"; Flags: ignoreversion

[Registry]
Root: HKLM; Subkey: "SYSTEM\CurrentControlSet\Control\Session Manager\Environment"; ValueType: expandsz; ValueName: "Path"; ValueData: "{olddata};{app}"; Check: NeedsAddPath(ExpandConstant('{app}'))

[Code]
function NeedsAddPath(Path: String): Boolean;
var
  CurrentPath: String;
begin
  if not RegQueryStringValue(HKLM,
    'SYSTEM\CurrentControlSet\Control\Session Manager\Environment',
    'Path', CurrentPath) then
    CurrentPath := '';
  Result := Pos(';' + Uppercase(Path) + ';',
    ';' + Uppercase(CurrentPath) + ';') = 0;
end;
