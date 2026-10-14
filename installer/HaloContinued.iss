; Compile with tools/package-installer.ps1. The portable ZIP supplies the payload.
#ifndef PackageDir
  #error PackageDir must point to a freshly extracted portable package.
#endif
#ifndef ReleaseVersion
  #define ReleaseVersion "1.0.0"
#endif
#ifndef NumericVersion
  #define NumericVersion "1.0.0"
#endif
#ifndef DestinationDir
  #define DestinationDir "..\dist"
#endif

#define ProductName "Halo Continued"
#define ProductId "{{D31E62BF-458D-43EA-A5BB-6B69A1717F78}"
#define ProductMutex "Local\HaloBatteryNext"
#define StartupValue "HaloBatteryNext"

[Setup]
AppId={#ProductId}
AppName={#ProductName}
AppVersion={#ReleaseVersion}
VersionInfoVersion={#NumericVersion}
AppPublisher=Duplex4773
AppPublisherURL=https://github.com/Duplex4773/HaloContinued
AppSupportURL=https://github.com/Duplex4773/HaloContinued/issues
AppUpdatesURL=https://github.com/Duplex4773/HaloContinued/releases
DefaultDirName={localappdata}\Programs\HaloContinued
DisableDirPage=yes
DisableProgramGroupPage=yes
PrivilegesRequired=lowest
ArchitecturesAllowed=x64compatible and not arm64
ArchitecturesInstallIn64BitMode=x64compatible
MinVersion=10.0.22000
AppMutex={#ProductMutex}
SetupMutex=Local\HaloContinued.Setup
CloseApplications=no
RestartApplications=no
RestartIfNeededByRun=no
UninstallDisplayIcon={app}\HaloContinued.exe
UninstallDisplayName={#ProductName}
SetupIconFile=..\crates\app\app.ico
LicenseFile={#PackageDir}\LICENSE-HaloBattery.txt
OutputDir={#DestinationDir}
OutputBaseFilename=HaloContinued-{#ReleaseVersion}-windows-x64-setup
WizardStyle=modern
Compression=lzma2
SolidCompression=yes
SignedUninstaller=no

[Languages]
Name: "english"; MessagesFile: "compiler:Default.isl"

[Messages]
SetupAppRunningError=Halo Continued is running.%n%nRight-click a Halo Continued icon in the notification area and choose Exit Halo Continued. Closing its window keeps monitoring active.%n%nThen click OK to continue, or Cancel to leave Setup.
UninstallAppRunningError=Halo Continued is running.%n%nRight-click a Halo Continued icon in the notification area and choose Exit Halo Continued. Closing its window keeps monitoring active.%n%nThen click OK to continue, or Cancel to leave Uninstall.
FinishedLabel=Halo Continued has been installed.%n%nYour settings and battery history are kept when you upgrade or uninstall.

[Tasks]
Name: "desktopicon"; Description: "Create a &desktop shortcut"; GroupDescription: "Shortcuts:"; Flags: unchecked
Name: "startup"; Description: "Start Halo Continued when I &sign in"; GroupDescription: "Startup:"; Flags: unchecked

[Files]
Source: "{#PackageDir}\HaloContinued.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#PackageDir}\README.md"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#PackageDir}\LICENSE-HaloBattery.txt"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#PackageDir}\THIRD-PARTY-NOTICES.txt"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#PackageDir}\SHA256SUMS.txt"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#PackageDir}\licenses\*"; DestDir: "{app}\licenses"; Flags: ignoreversion

[Icons]
Name: "{autoprograms}\{#ProductName}"; Filename: "{app}\HaloContinued.exe"; AppUserModelID: "HaloBatteryNext.Desktop"
Name: "{autodesktop}\{#ProductName}"; Filename: "{app}\HaloContinued.exe"; Tasks: desktopicon; AppUserModelID: "HaloBatteryNext.Desktop"

[Run]
Filename: "{app}\HaloContinued.exe"; Description: "Open Halo Continued"; Flags: nowait postinstall skipifsilent

[Code]
const
  StartupKey = 'Software\Microsoft\Windows\CurrentVersion\Run';
  StartupName = '{#StartupValue}';

var
  StartupChoiceInitialised: Boolean;

function StartupCommand: String;
begin
  Result := '"' + ExpandConstant('{app}\HaloContinued.exe') + '" --background';
end;

function StartupOwnedByInstall: Boolean;
var
  Command: String;
begin
  Result := RegQueryStringValue(HKCU, StartupKey, StartupName, Command) and
    (CompareText(Trim(Command), StartupCommand) = 0);
end;

procedure RemoveOwnedStartup;
begin
  // A user may have switched startup to a portable copy since installation.
  if StartupOwnedByInstall then
    RegDeleteValue(HKCU, StartupKey, StartupName);
end;

procedure InitialiseStartupChoice;
var
  I: Integer;
  Argument: String;
begin
  if StartupChoiceInitialised then
    Exit;
  StartupChoiceInitialised := True;
  // Honour explicit unattended task choices. Otherwise follow the app's current
  // setting, including changes made in Settings after an earlier installation.
  for I := 1 to ParamCount do begin
    Argument := Lowercase(ParamStr(I));
    if (Pos('/tasks=', Argument) = 1) or (Pos('/mergetasks=', Argument) = 1) then
      Exit;
  end;
  if StartupOwnedByInstall then
    WizardSelectTasks('startup')
  else
    WizardSelectTasks('!startup');
end;

procedure CurPageChanged(CurPageID: Integer);
begin
  if CurPageID = wpSelectTasks then
    InitialiseStartupChoice;
end;

procedure CurStepChanged(CurStep: TSetupStep);
begin
  if CurStep = ssInstall then
    InitialiseStartupChoice;
  if CurStep = ssPostInstall then begin
    if WizardIsTaskSelected('startup') then begin
      if not RegWriteStringValue(HKCU, StartupKey, StartupName, StartupCommand) then
        MsgBox('Halo Continued was installed, but could not enable Start with Windows. You can try again in Settings.', mbInformation, MB_OK);
    end else
      RemoveOwnedStartup;
  end;
end;

procedure CurUninstallStepChanged(CurUninstallStep: TUninstallStep);
begin
  if CurUninstallStep = usPostUninstall then
    RemoveOwnedStartup;
  // No user-data deletion: normal settings, history and portable data stay intact.
end;
