#ifndef PayloadRoot
  #error PayloadRoot must identify the explicitly staged installer payload
#endif
#ifndef SourceIdentity
  #error SourceIdentity must identify the explicitly staged source
#endif

#define SourceIdentityHex Copy(SourceIdentity, 1, 9) == "WORKTREE-" ? Copy(SourceIdentity, 10) : SourceIdentity
#if !((Len(SourceIdentity) == 40) || ((Len(SourceIdentity) == 73) && (Copy(SourceIdentity, 1, 9) == "WORKTREE-")))
  #error SourceIdentity must be 40 lowercase hex or WORKTREE- followed by 64 lowercase hex
#endif
#define SourceIdentityIndex
#sub ValidateSourceIdentityCharacter
  #if Pos(Copy(SourceIdentityHex, SourceIdentityIndex, 1), "0123456789abcdef") == 0
    #error SourceIdentity contains a non-lowercase-hex character
  #endif
#endsub
#for {SourceIdentityIndex = 1; SourceIdentityIndex <= Len(SourceIdentityHex); SourceIdentityIndex++} ValidateSourceIdentityCharacter

#define SourceMarkerPath AddBackslash(PayloadRoot) + "SOURCE-COMMIT.txt"
#define SourceMarkerHandle FileOpen(SourceMarkerPath)
#if !SourceMarkerHandle
  #error SOURCE-COMMIT.txt must be an explicitly staged installer input
#endif
#define SourceMarkerIdentity FileRead(SourceMarkerHandle)
#if !FileEof(SourceMarkerHandle)
  #error SOURCE-COMMIT.txt must contain exactly one line
#endif
#expr FileClose(SourceMarkerHandle)
#if SourceMarkerIdentity != SourceIdentity
  #error SOURCE-COMMIT.txt does not match SourceIdentity
#endif

#define AppName "LCDSirPlus"
#define AppVersion "0.3.0"
#define AppId "{EEC8142F-9507-4D89-AF3F-D6BDD0804E5E}"
#define StartupTaskPrefix "LCDSirPlus_EEC8142F-9507-4D89-AF3F-D6BDD0804E5E_Startup_"
#define TaskIdentity "LCDSirPlus {EEC8142F-9507-4D89-AF3F-D6BDD0804E5E}"
#define TaskDescription "LCDSirPlus interactive startup task"
#define UninstallKey "Software\Microsoft\Windows\CurrentVersion\Uninstall\{EEC8142F-9507-4D89-AF3F-D6BDD0804E5E}_is1"
#define OwnerMetadataName "LCDSirPlus-Owner.txt"
#define OwnerMetadataHeader "LCDSirPlus installer owner v1"

[Setup]
AppId={{#AppId}
AppName={#AppName}
AppVersion={#AppVersion}
AppVerName={#AppName} {#AppVersion}
AppPublisher=LCDSirPlus contributors
AppPublisherURL=https://github.com/InsecurePassword/LCDSirPlus
AppSupportURL=https://github.com/InsecurePassword/LCDSirPlus
AppUpdatesURL=https://github.com/InsecurePassword/LCDSirPlus/releases
AppModifyPath={code:GetModifyPath}
VersionInfoVersion=0.3.0.0
VersionInfoCompany=LCDSirPlus contributors
VersionInfoDescription=LCDSirPlus Setup
VersionInfoProductName=LCDSirPlus
VersionInfoProductVersion=0.3.0
DefaultDirName={autopf}\LCDSirPlus
DefaultGroupName=LCDSirPlus
DisableProgramGroupPage=auto
AllowNoIcons=yes
PrivilegesRequired=lowest
PrivilegesRequiredOverridesAllowed=dialog
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
MinVersion=10.0.22000
UninstallDisplayIcon={app}\LCDSirPlus.exe
AppMutex=Local\LCDSirPlus.Runtime
CloseApplications=yes
RestartApplications=yes
RestartIfNeededByRun=no
Compression=lzma2/max
CompressionThreads=1
LZMANumBlockThreads=1
SolidCompression=yes
Encryption=no
OutputDir=.
OutputBaseFilename=LCDSirPlus-0.3.0-win-x64-setup
SetupLogging=yes
WizardStyle=modern

; ModifyPath reruns the cached exact setup with its real Inno scope switch.

[Tasks]
Name: "startmenu"; Description: "Create Start Menu shortcuts"; GroupDescription: "Shortcuts:"
Name: "desktopicon"; Description: "Create a desktop shortcut"; GroupDescription: "Shortcuts:"; Flags: unchecked
Name: "startup"; Description: "Start LCDSirPlus when I sign in"; GroupDescription: "Startup:"

[Files]
Source: "{#PayloadRoot}\LCDSirPlus.exe"; DestDir: "{app}"; Flags: ignoreversion notimestamp
Source: "{#PayloadRoot}\PresentMon.exe"; DestDir: "{app}"; Flags: ignoreversion notimestamp
Source: "{#PayloadRoot}\lcdsirplus.default.txt"; DestDir: "{app}"; Flags: ignoreversion notimestamp
Source: "{#PayloadRoot}\lcdsirplus.layout"; DestDir: "{app}"; Flags: ignoreversion notimestamp
Source: "{#PayloadRoot}\SOURCE-COMMIT.txt"; DestDir: "{app}"; Attribs: readonly; Flags: overwritereadonly uninsremovereadonly ignoreversion notimestamp
Source: "{#PayloadRoot}\README.md"; DestDir: "{app}"; Flags: ignoreversion notimestamp
Source: "{#PayloadRoot}\modules.md"; DestDir: "{app}"; Flags: ignoreversion notimestamp
Source: "{#PayloadRoot}\LICENSE"; DestDir: "{app}"; Flags: ignoreversion notimestamp
Source: "{#PayloadRoot}\THIRD_PARTY_LICENSES.txt"; DestDir: "{app}"; Flags: ignoreversion notimestamp
Source: "{#PayloadRoot}\docs\CONFIGURATION.md"; DestDir: "{app}\docs"; Flags: ignoreversion notimestamp
Source: "{#PayloadRoot}\docs\INSTRUCTION-MANUAL.md"; DestDir: "{app}\docs"; Flags: ignoreversion notimestamp
Source: "{#PayloadRoot}\docs\LCDSirPlus-Instruction-Manual.pdf"; DestDir: "{app}\docs"; Flags: ignoreversion notimestamp
Source: "{#PayloadRoot}\licenses\PresentMon\LICENSE.txt"; DestDir: "{app}\licenses\PresentMon"; Flags: ignoreversion notimestamp
Source: "{#PayloadRoot}\licenses\PresentMon\THIRD_PARTY.txt"; DestDir: "{app}\licenses\PresentMon"; Flags: ignoreversion notimestamp

[Icons]
Name: "{autoprograms}\LCDSirPlus\LCDSirPlus"; Filename: "{app}\LCDSirPlus.exe"; WorkingDir: "{app}"; Tasks: startmenu
Name: "{autoprograms}\LCDSirPlus\Uninstall LCDSirPlus"; Filename: "{uninstallexe}"; Tasks: startmenu
Name: "{autodesktop}\LCDSirPlus"; Filename: "{app}\LCDSirPlus.exe"; WorkingDir: "{app}"; Tasks: desktopicon

[UninstallDelete]
Type: files; Name: "{app}\installer\LCDSirPlus-Setup.exe"
Type: files; Name: "{app}\installer\{#OwnerMetadataName}"
Type: dirifempty; Name: "{app}\installer"

[Code]
const
  UninstallKey = '{#UninstallKey}';
  MoveFileReplaceExisting = 1;
  MoveFileWriteThrough = 8;

function MoveFileEx(ExistingFileName, NewFileName: String;
  Flags: Cardinal): Boolean;
  external 'MoveFileExW@kernel32.dll stdcall';

var
  PreviousInstallDir, CurrentUserSid, CurrentTaskName, PriorCacheCopy,
    PriorOwnerCopy, UninstallTaskSid, UninstallTaskName: String;
  HadPriorCache, HadPriorOwner, SourceIsCache: Boolean;

procedure AddScriptLine(var Script: String; const Line: String);
begin
  Script := Script + Line + #13#10;
end;

function CopyFileAtomic(const Source, Destination: String): Boolean;
var
  PendingPath: String;
begin
  PendingPath := Destination + '.new';
  DeleteFile(PendingPath);
  Result := CopyFile(Source, PendingPath, False);
  if Result then
    Result := MoveFileEx(PendingPath, Destination,
      MoveFileReplaceExisting or MoveFileWriteThrough);
  if not Result then
    DeleteFile(PendingPath);
end;

function QuoteWindowsArg(const Value: String): String;
var
  I, J, Backslashes: Integer;
begin
  for I := 1 to Length(Value) do
    if (Value[I] = #0) or (Value[I] = #13) or (Value[I] = #10) then
      RaiseException('NUL, CR, and LF are not allowed in helper arguments.');
  Result := '"';
  Backslashes := 0;
  for I := 1 to Length(Value) do begin
    if Value[I] = '\' then
      Backslashes := Backslashes + 1
    else begin
      if Value[I] = '"' then begin
        for J := 1 to (Backslashes * 2) + 1 do
          Result := Result + '\';
        Result := Result + '"';
      end
      else begin
        for J := 1 to Backslashes do
          Result := Result + '\';
        Result := Result + Value[I];
      end;
      Backslashes := 0;
    end;
  end;
  for J := 1 to Backslashes * 2 do
    Result := Result + '\';
  Result := Result + '"';
end;

function IsSidValue(const Value: String): Boolean;
var
  I: Integer;
begin
  Result := False;
  if Length(Value) < 7 then
    Exit;
  Result := (CompareText(Copy(Value, 1, 4), 'S-1-') = 0) and
    (Value[5] <> '-') and (Value[Length(Value)] <> '-') and
    (Pos('-', Copy(Value, 5, Length(Value))) > 0);
  if not Result then
    Exit;
  for I := 5 to Length(Value) do begin
    if not (((Value[I] >= '0') and (Value[I] <= '9')) or
        (Value[I] = '-')) or
        ((Value[I] = '-') and (Value[I - 1] = '-')) then begin
      Result := False;
      Exit;
    end;
  end;
end;

function ScopeRoot: Integer;
begin
  if IsAdminInstallMode then
    Result := HKLM64
  else
    Result := HKCU;
end;

function GetModifyPath(const Param: String): String;
var
  ScopeSwitch: String;
begin
  if IsAdminInstallMode then
    ScopeSwitch := '/ALLUSERS'
  else
    ScopeSwitch := '/CURRENTUSER';
  Result := '"' + ExpandConstant('{app}\installer\LCDSirPlus-Setup.exe') +
    '" ' + ScopeSwitch;
end;

function OwnerMetadataPath(const InstallDir: String): String;
begin
  Result := AddBackslash(InstallDir) + 'installer\{#OwnerMetadataName}';
end;

function ReadOwnerMetadata(const InstallDir: String; var OwnerSid,
  TaskName: String): Boolean;
var
  Data: AnsiString;
  Text, Prefix, Tail: String;
  LineEnd: Integer;
begin
  Result := False;
  OwnerSid := '';
  TaskName := '';
  if not LoadStringFromFile(OwnerMetadataPath(InstallDir), Data) then
    Exit;
  Text := String(Data);
  Prefix := '{#OwnerMetadataHeader}' + #13#10;
  if Copy(Text, 1, Length(Prefix)) <> Prefix then
    Exit;
  Tail := Copy(Text, Length(Prefix) + 1, Length(Text));
  LineEnd := Pos(#13#10, Tail);
  if LineEnd <= 1 then
    Exit;
  OwnerSid := Copy(Tail, 1, LineEnd - 1);
  TaskName := Copy(Tail, LineEnd + 2, Length(Tail));
  if (Length(TaskName) < 2) or
      (Copy(TaskName, Length(TaskName) - 1, 2) <> #13#10) then
    Exit;
  Delete(TaskName, Length(TaskName) - 1, 2);
  if (Pos(#13, TaskName) <> 0) or (Pos(#10, TaskName) <> 0) or
      not IsSidValue(OwnerSid) then
    Exit;
  Result := (TaskName = '') or
    (CompareText(TaskName, '{#StartupTaskPrefix}' + OwnerSid) = 0);
end;

function WriteOwnerMetadata(const OwnerSid, TaskName: String): Boolean;
var
  Content, VerifiedSid, VerifiedTask: String;
begin
  Content := '{#OwnerMetadataHeader}' + #13#10 + OwnerSid + #13#10 +
    TaskName + #13#10;
  Result := SaveStringToFile(OwnerMetadataPath(ExpandConstant('{app}')),
    AnsiString(Content), False) and
    ReadOwnerMetadata(ExpandConstant('{app}'), VerifiedSid, VerifiedTask) and
    (CompareText(VerifiedSid, OwnerSid) = 0) and
    (CompareText(VerifiedTask, TaskName) = 0);
end;

function TaskScript: String;
var
  Script: String;
begin
  Script := '';
  AddScriptLine(Script, 'Option Explicit');
  AddScriptLine(Script, 'Const TaskNotFound = -2147024894');
  AddScriptLine(Script, 'Dim service, root, existing, definition, trigger, action, found, errorCode, fso, outputFile, shell, whoami, sidText, sidPattern, sidMatch');
  AddScriptLine(Script, 'Dim Mode, ExpectedPath, PreviousPath, ExpectedSid, ExpectedTaskName, SidOutput, NameOutput, TaskName, ProductIdentity, ProductDescription, TaskPrefix, currentSid');
  AddScriptLine(Script, 'Dim priorXml, priorUser, priorLogon, rollbackOk');
  AddScriptLine(Script, 'If WScript.Arguments.Count <> 10 Then WScript.Quit 26');
  AddScriptLine(Script, 'Mode = WScript.Arguments(0)');
  AddScriptLine(Script, 'ExpectedPath = WScript.Arguments(1)');
  AddScriptLine(Script, 'PreviousPath = WScript.Arguments(2)');
  AddScriptLine(Script, 'ExpectedSid = WScript.Arguments(3)');
  AddScriptLine(Script, 'ExpectedTaskName = WScript.Arguments(4)');
  AddScriptLine(Script, 'SidOutput = WScript.Arguments(5)');
  AddScriptLine(Script, 'NameOutput = WScript.Arguments(6)');
  AddScriptLine(Script, 'ProductIdentity = WScript.Arguments(7)');
  AddScriptLine(Script, 'ProductDescription = WScript.Arguments(8)');
  AddScriptLine(Script, 'TaskPrefix = WScript.Arguments(9)');
  AddScriptLine(Script, 'If Mode = "validate-uninstall" Or Mode = "uninstall-delete" Then');
  AddScriptLine(Script, '  If ExpectedSid = "" Or ExpectedTaskName = "" Then WScript.Quit 25');
  AddScriptLine(Script, '  Set sidPattern = New RegExp');
  AddScriptLine(Script, '  sidPattern.Pattern = "^S-1-[0-9]+(-[0-9]+)+$"');
  AddScriptLine(Script, '  If Not sidPattern.Test(ExpectedSid) Then WScript.Quit 25');
  AddScriptLine(Script, '  currentSid = ExpectedSid');
  AddScriptLine(Script, 'Else');
  AddScriptLine(Script, '  Set shell = CreateObject("WScript.Shell")');
  AddScriptLine(Script, '  Set whoami = shell.Exec("""" & shell.ExpandEnvironmentStrings("%SystemRoot%\System32\whoami.exe") & """ /user /fo csv /nh")');
  AddScriptLine(Script, '  sidText = whoami.StdOut.ReadAll');
  AddScriptLine(Script, '  If whoami.ExitCode <> 0 Then WScript.Quit 24');
  AddScriptLine(Script, '  Set sidPattern = New RegExp');
  AddScriptLine(Script, '  sidPattern.Pattern = """(S-1-[0-9-]+)"""');
  AddScriptLine(Script, '  Set sidMatch = sidPattern.Execute(sidText)');
  AddScriptLine(Script, '  If sidMatch.Count <> 1 Then WScript.Quit 24');
  AddScriptLine(Script, '  currentSid = sidMatch(0).SubMatches(0)');
  AddScriptLine(Script, 'End If');
  AddScriptLine(Script, 'TaskName = TaskPrefix & currentSid');
  AddScriptLine(Script, 'If ExpectedSid <> "" And Not SameText(ExpectedSid, currentSid) Then WScript.Quit 25');
  AddScriptLine(Script, 'If ExpectedTaskName <> "" And Not SameText(ExpectedTaskName, TaskName) Then WScript.Quit 25');
  AddScriptLine(Script, 'ExpectedSid = currentSid');
  AddScriptLine(Script, 'ExpectedTaskName = TaskName');
  AddScriptLine(Script, 'Set fso = CreateObject("Scripting.FileSystemObject")');
  AddScriptLine(Script, 'Set outputFile = fso.CreateTextFile(SidOutput, True, False)');
  AddScriptLine(Script, 'outputFile.Write ExpectedSid');
  AddScriptLine(Script, 'outputFile.Close');
  AddScriptLine(Script, 'Set outputFile = fso.CreateTextFile(NameOutput, True, False)');
  AddScriptLine(Script, 'outputFile.Write ExpectedTaskName');
  AddScriptLine(Script, 'outputFile.Close');
  AddScriptLine(Script, 'If Mode = "identify" Then WScript.Quit 0');
  AddScriptLine(Script, 'Set fso = CreateObject("Scripting.FileSystemObject")');
  AddScriptLine(Script, 'Function SameText(LeftValue, RightValue)');
  AddScriptLine(Script, '  SameText = (StrComp(CStr(LeftValue), CStr(RightValue), 1) = 0)');
  AddScriptLine(Script, 'End Function');
  AddScriptLine(Script, 'Function WqlLiteral(Value)');
  AddScriptLine(Script, '  WqlLiteral = "''" & Replace(CStr(Value), "''", "''''") & "''"');
  AddScriptLine(Script, 'End Function');
  AddScriptLine(Script, 'Function ResolveAccountSid(Identity)');
  AddScriptLine(Script, '  Dim slashAt, accountDomain, accountName, query, accounts, account, count');
  AddScriptLine(Script, '  ResolveAccountSid = ""');
  AddScriptLine(Script, '  If sidPattern.Test(CStr(Identity)) Then ResolveAccountSid = CStr(Identity): Exit Function');
  AddScriptLine(Script, '  slashAt = InStrRev(CStr(Identity), "\")');
  AddScriptLine(Script, '  If slashAt <= 1 Or slashAt = Len(CStr(Identity)) Then Exit Function');
  AddScriptLine(Script, '  accountDomain = Left(CStr(Identity), slashAt - 1)');
  AddScriptLine(Script, '  accountName = Mid(CStr(Identity), slashAt + 1)');
  AddScriptLine(Script, '  query = "SELECT SID FROM Win32_UserAccount WHERE Domain=" & WqlLiteral(accountDomain) & " AND Name=" & WqlLiteral(accountName)');
  AddScriptLine(Script, '  On Error Resume Next');
  AddScriptLine(Script, '  Set accounts = GetObject("winmgmts:root\cimv2").ExecQuery(query)');
  AddScriptLine(Script, '  If Err.Number <> 0 Then Err.Clear: On Error GoTo 0: Exit Function');
  AddScriptLine(Script, '  For Each account In accounts');
  AddScriptLine(Script, '    count = count + 1');
  AddScriptLine(Script, '    ResolveAccountSid = CStr(account.SID)');
  AddScriptLine(Script, '  Next');
  AddScriptLine(Script, '  If Err.Number <> 0 Or count <> 1 Then ResolveAccountSid = ""');
  AddScriptLine(Script, '  Err.Clear');
  AddScriptLine(Script, '  On Error GoTo 0');
  AddScriptLine(Script, 'End Function');
  AddScriptLine(Script, 'Function IsOwned(candidate)');
  AddScriptLine(Script, '  Dim candidateAction, candidateTrigger, ownedPath, xml, principalNodes, triggerNodes');
  AddScriptLine(Script, '  On Error Resume Next');
  AddScriptLine(Script, '  IsOwned = False');
  AddScriptLine(Script, '  If Not SameText(candidate.RegistrationInfo.Source, ProductIdentity) Then Exit Function');
  AddScriptLine(Script, '  If Not SameText(candidate.RegistrationInfo.Description, ProductDescription) Then Exit Function');
  AddScriptLine(Script, '  If candidate.Actions.Count <> 1 Then Exit Function');
  AddScriptLine(Script, '  Set candidateAction = candidate.Actions.Item(1)');
  AddScriptLine(Script, '  If Err.Number <> 0 Or candidateAction.Type <> 0 Then Err.Clear: Exit Function');
  AddScriptLine(Script, '  If CStr(candidateAction.Arguments) <> "" Then Exit Function');
  AddScriptLine(Script, '  ownedPath = SameText(candidateAction.Path, ExpectedPath) And SameText(candidateAction.WorkingDirectory, fso.GetParentFolderName(ExpectedPath))');
  AddScriptLine(Script, '  If Not ownedPath And PreviousPath <> "" Then ownedPath = SameText(candidateAction.Path, PreviousPath) And SameText(candidateAction.WorkingDirectory, fso.GetParentFolderName(PreviousPath))');
  AddScriptLine(Script, '  If Not ownedPath Then Exit Function');
  AddScriptLine(Script, '  If candidate.Triggers.Count <> 1 Then Exit Function');
  AddScriptLine(Script, '  Set candidateTrigger = candidate.Triggers.Item(1)');
  AddScriptLine(Script, '  If Err.Number <> 0 Or candidateTrigger.Type <> 9 Then Err.Clear: Exit Function');
  AddScriptLine(Script, '  If candidate.Principal.LogonType <> 3 Or candidate.Principal.RunLevel <> 0 Then Exit Function');
  AddScriptLine(Script, '  Set xml = CreateObject("Msxml2.DOMDocument.6.0")');
  AddScriptLine(Script, '  xml.async = False');
  AddScriptLine(Script, '  If Not xml.loadXML(candidate.XmlText) Then Exit Function');
  AddScriptLine(Script, '  xml.setProperty "SelectionNamespaces", "xmlns:t=''http://schemas.microsoft.com/windows/2004/02/mit/task''"');
  AddScriptLine(Script, '  Set principalNodes = xml.selectNodes("/t:Task/t:Principals/t:Principal/t:UserId")');
  AddScriptLine(Script, '  Set triggerNodes = xml.selectNodes("/t:Task/t:Triggers/t:LogonTrigger/t:UserId")');
  AddScriptLine(Script, '  If principalNodes.Length <> 1 Or triggerNodes.Length <> 1 Then Exit Function');
  AddScriptLine(Script, '  If ExpectedSid = "" Or Not SameText(principalNodes.Item(0).Text, ExpectedSid) Then Exit Function');
  AddScriptLine(Script, '  If Not SameText(ResolveAccountSid(triggerNodes.Item(0).Text), ExpectedSid) Then Exit Function');
  AddScriptLine(Script, '  If Err.Number <> 0 Then Err.Clear: Exit Function');
  AddScriptLine(Script, '  IsOwned = True');
  AddScriptLine(Script, 'End Function');
  AddScriptLine(Script, 'Function RestorePrior()');
  AddScriptLine(Script, '  Dim restored, restoreError');
  AddScriptLine(Script, '  On Error Resume Next');
  AddScriptLine(Script, '  root.RegisterTask TaskName, priorXml, 6, priorUser, Empty, priorLogon');
  AddScriptLine(Script, '  restoreError = Err.Number');
  AddScriptLine(Script, '  Err.Clear');
  AddScriptLine(Script, '  Set restored = root.GetTask(TaskName)');
  AddScriptLine(Script, '  If Err.Number <> 0 Then restoreError = Err.Number');
  AddScriptLine(Script, '  Err.Clear');
  AddScriptLine(Script, '  On Error GoTo 0');
  AddScriptLine(Script, '  If restoreError <> 0 Then RestorePrior = False: Exit Function');
  AddScriptLine(Script, '  RestorePrior = (CStr(restored.Xml) = CStr(priorXml))');
  AddScriptLine(Script, 'End Function');
  AddScriptLine(Script, 'Function CleanupNew()');
  AddScriptLine(Script, '  Dim created, cleanupError, deleteError, queryError');
  AddScriptLine(Script, '  On Error Resume Next');
  AddScriptLine(Script, '  Set created = root.GetTask(TaskName)');
  AddScriptLine(Script, '  cleanupError = Err.Number');
  AddScriptLine(Script, '  Err.Clear');
  AddScriptLine(Script, '  On Error GoTo 0');
  AddScriptLine(Script, '  If cleanupError = TaskNotFound Then CleanupNew = True: Exit Function');
  AddScriptLine(Script, '  If cleanupError <> 0 Then CleanupNew = False: Exit Function');
  AddScriptLine(Script, '  If Not IsOwned(created.Definition) Then CleanupNew = False: Exit Function');
  AddScriptLine(Script, '  On Error Resume Next');
  AddScriptLine(Script, '  root.DeleteTask TaskName, 0');
  AddScriptLine(Script, '  deleteError = Err.Number');
  AddScriptLine(Script, '  Err.Clear');
  AddScriptLine(Script, '  Set created = root.GetTask(TaskName)');
  AddScriptLine(Script, '  queryError = Err.Number');
  AddScriptLine(Script, '  Err.Clear');
  AddScriptLine(Script, '  On Error GoTo 0');
  AddScriptLine(Script, '  CleanupNew = (deleteError = 0 And queryError = TaskNotFound)');
  AddScriptLine(Script, 'End Function');
  AddScriptLine(Script, 'Set service = CreateObject("Schedule.Service")');
  AddScriptLine(Script, 'service.Connect');
  AddScriptLine(Script, 'Set root = service.GetFolder("\")');
  AddScriptLine(Script, 'If Err.Number <> 0 Then WScript.Quit 20');
  AddScriptLine(Script, 'On Error Resume Next');
  AddScriptLine(Script, 'Set existing = root.GetTask(TaskName)');
  AddScriptLine(Script, 'errorCode = Err.Number');
  AddScriptLine(Script, 'Err.Clear');
  AddScriptLine(Script, 'On Error GoTo 0');
  AddScriptLine(Script, 'If errorCode = 0 Then');
  AddScriptLine(Script, '  found = True');
  AddScriptLine(Script, 'ElseIf errorCode = TaskNotFound Then');
  AddScriptLine(Script, '  found = False');
  AddScriptLine(Script, 'Else');
  AddScriptLine(Script, '  WScript.Quit 20');
  AddScriptLine(Script, 'End If');
  AddScriptLine(Script, 'If found Then');
  AddScriptLine(Script, '  If Not IsOwned(existing.Definition) Then WScript.Quit 21');
  AddScriptLine(Script, 'ElseIf Mode = "uninstall-delete" And ExpectedSid = "" Then');
  AddScriptLine(Script, '  WScript.Quit 0');
  AddScriptLine(Script, 'End If');
  AddScriptLine(Script, 'If found Then');
  AddScriptLine(Script, '  On Error Resume Next');
  AddScriptLine(Script, '  priorXml = existing.Xml');
  AddScriptLine(Script, '  priorUser = existing.Definition.Principal.UserId');
  AddScriptLine(Script, '  priorLogon = existing.Definition.Principal.LogonType');
  AddScriptLine(Script, '  errorCode = Err.Number');
  AddScriptLine(Script, '  Err.Clear');
  AddScriptLine(Script, '  On Error GoTo 0');
  AddScriptLine(Script, '  If errorCode <> 0 Then WScript.Quit 20');
  AddScriptLine(Script, 'End If');
  AddScriptLine(Script, 'If Mode = "create" Then');
  AddScriptLine(Script, '  Set definition = service.NewTask(0)');
  AddScriptLine(Script, '  definition.RegistrationInfo.Source = ProductIdentity');
  AddScriptLine(Script, '  definition.RegistrationInfo.Description = ProductDescription');
  AddScriptLine(Script, '  definition.Principal.UserId = currentSid');
  AddScriptLine(Script, '  definition.Principal.LogonType = 3');
  AddScriptLine(Script, '  definition.Principal.RunLevel = 0');
  AddScriptLine(Script, '  definition.Settings.Enabled = True');
  AddScriptLine(Script, '  definition.Settings.AllowDemandStart = True');
  AddScriptLine(Script, '  definition.Settings.DisallowStartIfOnBatteries = False');
  AddScriptLine(Script, '  definition.Settings.StopIfGoingOnBatteries = False');
  AddScriptLine(Script, '  definition.Settings.ExecutionTimeLimit = "PT0S"');
  AddScriptLine(Script, '  Set trigger = definition.Triggers.Create(9)');
  AddScriptLine(Script, '  trigger.UserId = currentSid');
  AddScriptLine(Script, '  Set action = definition.Actions.Create(0)');
  AddScriptLine(Script, '  action.Path = ExpectedPath');
  AddScriptLine(Script, '  action.Arguments = ""');
  AddScriptLine(Script, '  action.WorkingDirectory = fso.GetParentFolderName(ExpectedPath)');
  AddScriptLine(Script, '  On Error Resume Next');
  AddScriptLine(Script, '  root.RegisterTaskDefinition TaskName, definition, 6, currentSid, Empty, 3');
  AddScriptLine(Script, '  errorCode = Err.Number');
  AddScriptLine(Script, '  Err.Clear');
  AddScriptLine(Script, '  On Error GoTo 0');
  AddScriptLine(Script, '  If errorCode <> 0 Then');
  AddScriptLine(Script, '    If found Then rollbackOk = RestorePrior() Else rollbackOk = CleanupNew()');
  AddScriptLine(Script, '    If rollbackOk Then WScript.Quit 30 Else WScript.Quit 31');
  AddScriptLine(Script, '  End If');
  AddScriptLine(Script, '  On Error Resume Next');
  AddScriptLine(Script, '  Set existing = root.GetTask(TaskName)');
  AddScriptLine(Script, '  errorCode = Err.Number');
  AddScriptLine(Script, '  Err.Clear');
  AddScriptLine(Script, '  On Error GoTo 0');
  AddScriptLine(Script, '  PreviousPath = ""');
  AddScriptLine(Script, '  If errorCode <> 0 Then');
  AddScriptLine(Script, '    If found Then rollbackOk = RestorePrior() Else rollbackOk = CleanupNew()');
  AddScriptLine(Script, '    If rollbackOk Then WScript.Quit 32 Else WScript.Quit 33');
  AddScriptLine(Script, '  End If');
  AddScriptLine(Script, '  If Not IsOwned(existing.Definition) Then');
  AddScriptLine(Script, '    If found Then rollbackOk = RestorePrior() Else rollbackOk = CleanupNew()');
  AddScriptLine(Script, '    If rollbackOk Then WScript.Quit 32 Else WScript.Quit 33');
  AddScriptLine(Script, '  End If');
  AddScriptLine(Script, 'ElseIf (Mode = "setup-delete" Or Mode = "uninstall-delete") And found Then');
  AddScriptLine(Script, '  On Error Resume Next');
  AddScriptLine(Script, '  root.DeleteTask TaskName, 0');
  AddScriptLine(Script, '  errorCode = Err.Number');
  AddScriptLine(Script, '  Err.Clear');
  AddScriptLine(Script, '  On Error GoTo 0');
  AddScriptLine(Script, '  If errorCode <> 0 Then');
  AddScriptLine(Script, '    rollbackOk = RestorePrior()');
  AddScriptLine(Script, '    If rollbackOk Then WScript.Quit 34 Else WScript.Quit 35');
  AddScriptLine(Script, '  End If');
  AddScriptLine(Script, '  On Error Resume Next');
  AddScriptLine(Script, '  Set existing = root.GetTask(TaskName)');
  AddScriptLine(Script, '  errorCode = Err.Number');
  AddScriptLine(Script, '  Err.Clear');
  AddScriptLine(Script, '  On Error GoTo 0');
  AddScriptLine(Script, '  If errorCode <> TaskNotFound Then');
  AddScriptLine(Script, '    rollbackOk = RestorePrior()');
  AddScriptLine(Script, '    If rollbackOk Then WScript.Quit 36 Else WScript.Quit 37');
  AddScriptLine(Script, '  End If');
  AddScriptLine(Script, 'End If');
  AddScriptLine(Script, 'WScript.Quit 0');
  Result := Script;
end;

function RunTaskMutation(const Mode, ExpectedSid, ExpectedName: String;
  const AsOriginalUser: Boolean; var OwnerSid, TaskName, Failure: String): Boolean;
var
  ResultCode: Integer;
  ScriptPath, SidPath, NamePath, PreviousPath, Parameters: String;
  SidBytes, NameBytes: AnsiString;
begin
  OwnerSid := '';
  TaskName := '';
  Failure := '';
  ScriptPath := ExpandConstant('{tmp}\LCDSirPlus-Task.vbs');
  SidPath := ExpandConstant('{tmp}\LCDSirPlus-Task.sid');
  NamePath := ExpandConstant('{tmp}\LCDSirPlus-Task.name');
  PreviousPath := '';
  if PreviousInstallDir <> '' then
    PreviousPath := AddBackslash(PreviousInstallDir) + 'LCDSirPlus.exe';
  DeleteFile(SidPath);
  DeleteFile(NamePath);
  if not SaveStringToFile(ScriptPath, TaskScript, False) then begin
    Failure := 'could not prepare the Task Scheduler ownership helper';
    Result := False;
    Exit;
  end;
  Parameters := QuoteWindowsArg(ScriptPath) + ' ' + QuoteWindowsArg(Mode) +
    ' ' + QuoteWindowsArg(ExpandConstant('{app}\LCDSirPlus.exe')) +
    ' ' + QuoteWindowsArg(PreviousPath) + ' ' + QuoteWindowsArg(ExpectedSid) +
    ' ' + QuoteWindowsArg(ExpectedName) + ' ' + QuoteWindowsArg(SidPath) +
    ' ' + QuoteWindowsArg(NamePath) + ' ' + QuoteWindowsArg('{#TaskIdentity}') +
    ' ' + QuoteWindowsArg('{#TaskDescription}') +
    ' ' + QuoteWindowsArg('{#StartupTaskPrefix}');
  if AsOriginalUser then
    Result := ExecAsOriginalUser(ExpandConstant('{sys}\wscript.exe'),
      Parameters, ExpandConstant('{tmp}'), SW_HIDE,
      ewWaitUntilTerminated, ResultCode)
  else
    Result := Exec(ExpandConstant('{sys}\wscript.exe'),
      Parameters, ExpandConstant('{tmp}'), SW_HIDE,
      ewWaitUntilTerminated, ResultCode);
  DeleteFile(ScriptPath);
  if not Result then
    Failure := 'could not launch the Task Scheduler ownership helper'
  else if ResultCode = 20 then begin
    Failure := 'the existing startup task could not be queried';
    Result := False;
  end
  else if ResultCode = 21 then begin
    Failure := 'the startup task name is occupied by an unverified task; it was not changed';
    Result := False;
  end
  else if ResultCode = 22 then begin
    Failure := 'Task Scheduler refused the requested owned-task change';
    Result := False;
  end
  else if ResultCode = 23 then begin
    Failure := 'the startup task could not be verified after the requested change';
    Result := False;
  end
  else if ResultCode = 24 then begin
    Failure := 'the original user SID could not be resolved';
    Result := False;
  end
  else if ResultCode = 25 then begin
    Failure := 'the stored startup task SID or name does not match the original user';
    Result := False;
  end
  else if ResultCode = 30 then begin
    Failure := 'Task Scheduler rejected registration; the exact prior task was restored';
    Result := False;
  end
  else if ResultCode = 31 then begin
    Failure := 'Task Scheduler rejected registration and rollback of the prior/new task failed';
    Result := False;
  end
  else if ResultCode = 32 then begin
    Failure := 'post-registration verification failed; the exact prior task was restored';
    Result := False;
  end
  else if ResultCode = 33 then begin
    Failure := 'post-registration verification failed and rollback of the prior/new task failed';
    Result := False;
  end
  else if ResultCode = 34 then begin
    Failure := 'Task Scheduler refused deletion; the exact prior task was restored';
    Result := False;
  end
  else if ResultCode = 35 then begin
    Failure := 'Task Scheduler refused deletion and restoration of the exact prior task failed';
    Result := False;
  end
  else if ResultCode = 36 then begin
    Failure := 'post-deletion verification failed; the exact prior task was restored';
    Result := False;
  end
  else if ResultCode = 37 then begin
    Failure := 'post-deletion verification failed and restoration of the exact prior task failed';
    Result := False;
  end
  else if ResultCode <> 0 then begin
    Failure := Format('the Task Scheduler ownership helper failed with exit code %d', [ResultCode]);
    Result := False;
  end;
  if Result and (Mode = 'identify') then
    if not LoadStringFromFile(SidPath, SidBytes) or
        not LoadStringFromFile(NamePath, NameBytes) then begin
      Failure := 'the startup task SID/name metadata could not be read';
      Result := False;
    end
    else begin
      OwnerSid := Trim(String(SidBytes));
      TaskName := Trim(String(NameBytes));
      if (OwnerSid = '') or (TaskName = '') then begin
        Failure := 'the startup task SID/name metadata was empty';
        Result := False;
      end;
    end
  else if Result then begin
    OwnerSid := ExpectedSid;
    TaskName := ExpectedName;
  end;
  DeleteFile(SidPath);
  DeleteFile(NamePath);
end;

function OriginalUserHasRegistration(var QueryFailure: Boolean): Boolean;
var
  ResultCode: Integer;
  ScriptPath, Parameters: String;
begin
  QueryFailure := False;
  ScriptPath := ExpandConstant('{tmp}\LCDSirPlus-Scope.vbs');
  if not SaveStringToFile(ScriptPath,
      'Option Explicit' + #13#10 +
      'Dim shell, value, errorCode' + #13#10 +
      'If WScript.Arguments.Count <> 1 Then WScript.Quit 4' + #13#10 +
      'Set shell = CreateObject("WScript.Shell")' + #13#10 +
      'On Error Resume Next' + #13#10 +
      'value = shell.RegRead(WScript.Arguments(0))' + #13#10 +
      'errorCode = Err.Number' + #13#10 +
      'On Error GoTo 0' + #13#10 +
      'If errorCode = 0 Then WScript.Quit 0' + #13#10 +
      'If errorCode = -2147024894 Then WScript.Quit 2' + #13#10 +
      'WScript.Quit 3' + #13#10, False) then begin
    QueryFailure := True;
    Result := False;
    Exit;
  end;
  Parameters := QuoteWindowsArg(ScriptPath) + ' ' +
    QuoteWindowsArg('HKCU\{#UninstallKey}\DisplayName');
  Result := ExecAsOriginalUser(ExpandConstant('{sys}\wscript.exe'),
    Parameters, ExpandConstant('{tmp}'), SW_HIDE,
    ewWaitUntilTerminated, ResultCode);
  DeleteFile(ScriptPath);
  if not Result then begin
    QueryFailure := True;
    Result := False;
  end
  else if ResultCode = 0 then
    Result := True
  else if ResultCode = 2 then
    Result := False
  else begin
    QueryFailure := True;
    Result := False;
  end;
end;

function RollbackPreparedMetadata: Boolean;
var
  CachePath, OwnerPath: String;
begin
  Result := True;
  CachePath := ExpandConstant('{app}\installer\LCDSirPlus-Setup.exe');
  OwnerPath := OwnerMetadataPath(ExpandConstant('{app}'));
  if not SourceIsCache then begin
    if HadPriorCache then begin
      if not CopyFileAtomic(PriorCacheCopy, CachePath) then
        Result := False;
    end
    else begin
      if FileExists(CachePath) and not DeleteFile(CachePath) then
        Result := False;
    end;
  end;
  if HadPriorOwner then begin
    if not CopyFile(PriorOwnerCopy, OwnerPath, False) then
      Result := False;
  end
  else if FileExists(OwnerPath) and not DeleteFile(OwnerPath) then
    Result := False;
  RemoveDir(ExtractFileDir(CachePath));
end;

function PrepareToInstall(var NeedsRestart: Boolean): String;
var
  OppositeExists, QueryFailure, SameScopeExists: Boolean;
  AppDir, OppositeInstallDir, OppositeOwnerSid, OwnerSid, TaskName, Failure,
    CachePath, OwnerPath: String;
begin
  Result := '';
  PreviousInstallDir := '';
  if not RunTaskMutation('identify', '', '', True, CurrentUserSid,
      CurrentTaskName, Failure) then begin
    Result := 'Setup could not resolve the original user''s stable SID/task identity: ' +
      Failure + '.';
    Exit;
  end;
  if not IsSidValue(CurrentUserSid) then begin
    Result := 'Setup refused an invalid original-user SID.';
    Exit;
  end;
  AppDir := ExpandConstant('{app}');
  if FileExists(AddBackslash(AppDir) + 'INSTALL-MANIFEST.txt') or
      FileExists(AddBackslash(AppDir) + 'Install.ps1') or
      FileExists(AddBackslash(AppDir) + 'Uninstall.ps1') then begin
    Result := 'A legacy PowerShell LCDSirPlus installation exists at ' + AppDir + '. ' +
      'Cancel setup and run powershell.exe -NoProfile -File "' +
      AddBackslash(AppDir) + 'Uninstall.ps1" -InstallRoot "' + AppDir +
      '" first. Automatic migration is not supported.';
    Exit;
  end;

  SameScopeExists := RegKeyExists(ScopeRoot, UninstallKey);
  if SameScopeExists then begin
    if not RegQueryStringValue(ScopeRoot, UninstallKey, 'InstallLocation',
        PreviousInstallDir) or (Trim(PreviousInstallDir) = '') then begin
      Result := 'The existing same-scope LCDSirPlus uninstall registration is damaged. ' +
        'Uninstall it from Windows Installed apps before running setup again.';
      Exit;
    end
    end;

  if SameScopeExists then begin
    if not ReadOwnerMetadata(PreviousInstallDir, OppositeOwnerSid,
        TaskName) or
        (CompareText(OppositeOwnerSid, CurrentUserSid) <> 0) then begin
      Result := 'The existing same-scope LCDSirPlus owner metadata is invalid or belongs to another user SID. ' +
        'Uninstall it from its owner account before running setup again.';
      Exit;
    end;
  end;

  if IsAdminInstallMode then begin
    OppositeExists := OriginalUserHasRegistration(QueryFailure);
    if QueryFailure then begin
      Result := 'Setup could not safely query the original user''s LCDSirPlus registration. ' +
        'Start setup from that user''s normal desktop session and try again.';
      Exit;
    end;
  end
  else begin
    OppositeExists := RegKeyExists(HKLM64, UninstallKey);
    if OppositeExists then begin
      if not RegQueryStringValue(HKLM64, UninstallKey, 'InstallLocation',
          OppositeInstallDir) or (Trim(OppositeInstallDir) = '') or
          not ReadOwnerMetadata(OppositeInstallDir, OppositeOwnerSid,
          TaskName) then begin
        Result := 'Setup cannot authenticate the owner of the existing all-users LCDSirPlus registration. ' +
          'Uninstall it from its owner account before continuing.';
        Exit;
      end;
      OppositeExists := CompareText(OppositeOwnerSid, CurrentUserSid) = 0;
    end;
  end;
  if OppositeExists then begin
    Result := 'LCDSirPlus is already registered for this user SID in the opposite install scope. ' +
      'Uninstall that copy before changing scope. Registrations owned by other accounts may coexist.';
    Exit;
  end;
  if not RunTaskMutation('validate-setup', CurrentUserSid, CurrentTaskName,
      True, OwnerSid, TaskName, Failure) then begin
    Result := 'Setup cannot safely use the LCDSirPlus startup task name: ' +
      Failure + '. Resolve the Task Scheduler collision or query error, then retry.';
    Exit;
  end;

  CachePath := ExpandConstant('{app}\installer\LCDSirPlus-Setup.exe');
  OwnerPath := OwnerMetadataPath(ExpandConstant('{app}'));
  SourceIsCache := CompareText(ExpandFileName(ExpandConstant('{srcexe}')),
    ExpandFileName(CachePath)) = 0;
  HadPriorCache := FileExists(CachePath);
  HadPriorOwner := FileExists(OwnerPath);
  PriorCacheCopy := ExpandConstant('{tmp}\LCDSirPlus-Setup.previous.exe');
  PriorOwnerCopy := ExpandConstant('{tmp}\LCDSirPlus-Owner.previous.txt');
  DeleteFile(PriorCacheCopy);
  DeleteFile(PriorOwnerCopy);
  if HadPriorCache and not SourceIsCache and
      not CopyFile(CachePath, PriorCacheCopy, False) then
    Result := 'Setup could not preserve the existing cached installer before startup task mutation.'
  else if HadPriorOwner and not CopyFile(OwnerPath, PriorOwnerCopy, False) then
    Result := 'Setup could not preserve the existing owner metadata before startup task mutation.';
end;

procedure CurStepChanged(CurStep: TSetupStep);
var
  OwnerSid, TaskName, Failure, CachePath, MetadataTaskName: String;
  MetadataPrepared, MetadataRolledBack: Boolean;
begin
  if CurStep <> ssPostInstall then
    Exit;
  MetadataPrepared := True;
  CachePath := ExpandConstant('{app}\installer\LCDSirPlus-Setup.exe');
  if not ForceDirectories(ExtractFileDir(CachePath)) then
    MetadataPrepared := False;
  if not SourceIsCache and
      not CopyFileAtomic(ExpandConstant('{srcexe}'), CachePath) then
    MetadataPrepared := False;
  if not FileExists(CachePath) then
    MetadataPrepared := False;
  if WizardIsTaskSelected('startup') then
    MetadataTaskName := CurrentTaskName
  else
    MetadataTaskName := '';
  if MetadataPrepared and not WriteOwnerMetadata(CurrentUserSid,
      MetadataTaskName) then
    MetadataPrepared := False;

  if not MetadataPrepared then begin
    MetadataRolledBack := RollbackPreparedMetadata;
    if MetadataRolledBack then
      RaiseException('Could not prepare the setup cache and owner metadata file; prior files were restored.')
    else
      RaiseException('Could not prepare the setup cache and owner metadata file, and file rollback also failed.');
  end;

  if WizardIsTaskSelected('startup') then
    MetadataPrepared := RunTaskMutation('create', CurrentUserSid,
      CurrentTaskName, True, OwnerSid, TaskName, Failure)
  else
    MetadataPrepared := RunTaskMutation('setup-delete', CurrentUserSid,
      CurrentTaskName, True, OwnerSid, TaskName, Failure);
  if not MetadataPrepared then begin
    MetadataRolledBack := RollbackPreparedMetadata;
    if MetadataRolledBack then
      RaiseException('LCDSirPlus startup task change failed: ' + Failure +
        '. Prior setup metadata was restored.')
    else
      RaiseException('LCDSirPlus startup task change failed: ' + Failure +
        '. Setup metadata rollback also failed.');
  end;
end;

function InitializeUninstall: Boolean;
var
  OwnerSid, TaskName, Failure: String;
begin
  PreviousInstallDir := '';
  UninstallTaskSid := '';
  UninstallTaskName := '';
  if not ReadOwnerMetadata(ExpandConstant('{app}'), UninstallTaskSid,
      UninstallTaskName) then begin
    SuppressibleMsgBox('LCDSirPlus uninstall stopped because the protected owner metadata file could not be authenticated.',
      mbError, MB_OK, IDOK);
    Result := False;
    Exit;
  end;
  if UninstallTaskName = '' then begin
    Result := True;
    Exit;
  end;
  Result := RunTaskMutation('validate-uninstall', UninstallTaskSid,
    UninstallTaskName, False, OwnerSid, TaskName, Failure);
  if not Result then
    SuppressibleMsgBox('LCDSirPlus uninstall stopped during startup task validation: ' +
      Failure + '. Resolve the startup task collision or Task Scheduler error, then retry uninstall.',
      mbError, MB_OK, IDOK);
end;

procedure CurUninstallStepChanged(CurUninstallStep: TUninstallStep);
var
  OwnerSid, TaskName, Failure: String;
begin
  if (CurUninstallStep <> usUninstall) or (UninstallTaskName = '') then
    Exit;
  if not RunTaskMutation('uninstall-delete', UninstallTaskSid,
      UninstallTaskName, False, OwnerSid, TaskName, Failure) then
    RaiseException('LCDSirPlus uninstall stopped before removing application files: ' +
      Failure + '. Resolve the startup task collision or Task Scheduler error, then retry uninstall.');
end;
