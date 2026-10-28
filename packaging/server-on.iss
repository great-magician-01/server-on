#ifndef AppVersion
  #define AppVersion "1.0.0"
#endif
#ifndef NumericVersion
  #define NumericVersion "1.0.0.0"
#endif

[Setup]
AppId=ServerOn
AppName=ServerOn
AppVersion={#AppVersion}
AppPublisher=ServerOn
DefaultDirName={localappdata}\Programs\ServerOn
DefaultGroupName=ServerOn
DisableProgramGroupPage=yes
PrivilegesRequired=lowest
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
OutputDir=..\dist
OutputBaseFilename=ServerOn-Setup-{#AppVersion}-windows-x64
SetupIconFile=..\assets\icon\server-on.ico
UninstallDisplayIcon={app}\server-on.exe
AppMutex=Local\ServerOn.Gui
CloseApplications=no
RestartApplications=no
VersionInfoVersion={#NumericVersion}
VersionInfoProductVersion={#NumericVersion}
Compression=lzma2
SolidCompression=yes
WizardStyle=modern

[Files]
Source: "..\dist\ServerOn\server-on.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\dist\ServerOn\server-on-daemon.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\dist\ServerOn\server-on-cli.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\dist\ServerOn\README.md"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\dist\ServerOn\DESIGN.md"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\dist\ServerOn\VALIDATION.md"; DestDir: "{app}"; Flags: ignoreversion

[Icons]
Name: "{userprograms}\ServerOn"; Filename: "{app}\server-on.exe"; WorkingDir: "{app}"; IconFilename: "{app}\server-on.exe"

[Tasks]
Name: "startup"; Description: "登录 Windows 时启动 ServerOn daemon"; GroupDescription: "其他选项："; Flags: unchecked

[Registry]
Root: HKCU; Subkey: "Software\Microsoft\Windows\CurrentVersion\Run"; ValueType: string; ValueName: "ServerOnDaemon"; ValueData: """{app}\server-on-daemon.exe"""; Tasks: startup; Flags: uninsdeletevalue

[Run]
Filename: "{app}\server-on.exe"; Description: "启动 ServerOn"; Flags: nowait postinstall skipifsilent unchecked

[Code]
function IsProcessRunning(const ExeName: string): Boolean;
var
  ResultCode: Integer;
  OutputFile: string;
  Output: AnsiString;
begin
  Result := False;
  OutputFile := ExpandConstant('{tmp}\server-on-process-check.txt');
  if not Exec(
    ExpandConstant('{cmd}'),
    '/C tasklist /FI "IMAGENAME eq ' + ExeName + '" /FO CSV /NH > "' + OutputFile + '"',
    '',
    SW_HIDE,
    ewWaitUntilTerminated,
    ResultCode
  ) then
    Exit;

  if (ResultCode = 0) and LoadStringFromFile(OutputFile, Output) then
    Result := Pos(Uppercase(ExeName), Uppercase(String(Output))) > 0;
  DeleteFile(OutputFile);
end;

function RunningApplicationName: string;
begin
  Result := '';
  if IsProcessRunning('server-on.exe') then
    Result := 'ServerOn GUI'
  else if IsProcessRunning('server-on-daemon.exe') then
    Result := 'ServerOn daemon';
end;

function InitializeSetup: Boolean;
var
  ApplicationName: string;
begin
  Result := True;
  ApplicationName := RunningApplicationName;
  if ApplicationName <> '' then
  begin
    SuppressibleMsgBox(
      '检测到 ' + ApplicationName + ' 正在运行。请先关闭 GUI 和 daemon，再重新安装。' + #13#10 +
      '安装程序不会自动停止 daemon 或其管理的服务。',
      mbError,
      MB_OK,
      IDOK
    );
    Result := False;
  end;
end;

function InitializeUninstall: Boolean;
var
  ApplicationName: string;
begin
  Result := True;
  ApplicationName := RunningApplicationName;
  if ApplicationName <> '' then
  begin
    SuppressibleMsgBox(
      '检测到 ' + ApplicationName + ' 正在运行。请先关闭 GUI 和 daemon，再卸载。' + #13#10 +
      '卸载程序不会自动停止 daemon 或其管理的服务。',
      mbError,
      MB_OK,
      IDOK
    );
    Result := False;
  end;
end;

function RemoveDataRequested: Boolean;
var
  Index: Integer;
begin
  Result := False;
  for Index := 1 to ParamCount do
    if Uppercase(ParamStr(Index)) = '/REMOVEDATA' then
      Result := True;
end;

procedure CurUninstallStepChanged(CurUninstallStep: TUninstallStep);
var
  RemoveData: Boolean;
begin
  if CurUninstallStep <> usPostUninstall then
    Exit;

  RemoveData := RemoveDataRequested;
  if not UninstallSilent and not RemoveData then
    RemoveData := MsgBox(
      '是否同时删除 ServerOn 的配置、状态和日志？' + #13#10 +
      '选择“否”将保留 %LOCALAPPDATA%\ServerOn。',
      mbConfirmation,
      MB_YESNO or MB_DEFBUTTON2
    ) = IDYES;

  if RemoveData then
    DelTree(ExpandConstant('{localappdata}\ServerOn'), True, True, True);
end;
