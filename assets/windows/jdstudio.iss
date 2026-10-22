; JDStudio's Windows installer.
;
; Built by build-app.yml with Inno Setup from the staged folder the .msi
; is also made of, so the two ship the same files; this is the one a
; person double-clicks, and it puts JDStudio in the Start menu and can take
; it out again. Everything it needs is handed in on the command line:
;
;   iscc /DVersion=0.2.8 /DArch=x64compatible /DSuffix=x86_64 ^
;        /DStage=C:\...\stage\JDStudio-0.2.8-windows-x86_64 /DOut=C:\...\stage ^
;        assets\windows\jdstudio.iss
;
; Arch is Inno's own word for the machine: x64compatible for the x86_64
; build (which also installs on ARM PCs, under emulation), arm64 for the
; native one. Suffix is the bundle's word for it, which names the file.

#ifndef Version
  #error Version is required
#endif
#ifndef Arch
  #error Arch is required: x64compatible or arm64
#endif
#ifndef Suffix
  #error Suffix is required: x86_64 or aarch64
#endif
#ifndef Stage
  #error Stage is required: the staged folder to install
#endif
#ifndef Out
  #define Out "."
#endif

[Setup]
; One id for the life of the product, so an install over an older one is
; an upgrade and not a second copy.
AppId={{3D8F2A61-5C47-4E9B-A1D2-7E6B9C4F0A53}
AppName=JDStudio
AppVersion={#Version}
AppVerName=JDStudio {#Version}
AppPublisher=JDStudio contributors
AppPublisherURL=https://github.com/andyresta/jdstudio
AppSupportURL=https://github.com/andyresta/jdstudio/issues
AppUpdatesURL=https://github.com/andyresta/jdstudio/releases
DefaultDirName={autopf}\JDStudio
DefaultGroupName=JDStudio
DisableProgramGroupPage=yes
LicenseFile=..\..\LICENSE
OutputDir={#Out}
OutputBaseFilename=JDStudio-{#Version}-windows-{#Suffix}-setup
SetupIconFile=..\icons\jdstudio.ico
UninstallDisplayIcon={app}\jdstudio.ico
UninstallDisplayName=JDStudio
Compression=lzma2/max
SolidCompression=yes
WizardStyle=modern
ArchitecturesAllowed={#Arch}
ArchitecturesInstallIn64BitMode={#Arch}
; For the user alone unless they ask for the machine: no prompt for an
; administrator to install a video editor into one's own account.
PrivilegesRequired=lowest
PrivilegesRequiredOverridesAllowed=dialog
; Follow Windows' display language when the setup speaks it, and ask only
; when it does not.
ShowLanguageDialog=auto

[Languages]
; Every language the app ships. Inno Setup carries the official
; translations. The ones it keeps as unofficial - Chinese, Persian,
; Croatian, Bengali, Hindi, Indonesian, Urdu, Vietnamese - and the ones
; made official lately - Korean, Arabic and Thai, which an older compiler
; lacks - are vendored in languages/ from the Inno Setup 6.7.1 tree
; (https://github.com/jrsoftware/issrc/tree/is-6_7_1/Files/Languages),
; verbatim, so the setup offers them whatever the compiler on the build
; machine ships (#277). Some are in an older format, which the compiler
; still reads with the newer messages in English and a warning for each:
; Traditional Chinese 6.1, Urdu 6.1, Indonesian 6.4, and Bengali and Hindi
; 5.5.3, whose obsolete title and copyright font sizes it ignores.
Name: "english"; MessagesFile: "compiler:Default.isl"
Name: "german"; MessagesFile: "compiler:Languages\German.isl"
Name: "spanish"; MessagesFile: "compiler:Languages\Spanish.isl"
Name: "french"; MessagesFile: "compiler:Languages\French.isl"
Name: "italian"; MessagesFile: "compiler:Languages\Italian.isl"
Name: "japanese"; MessagesFile: "compiler:Languages\Japanese.isl"
Name: "brazilianportuguese"; MessagesFile: "compiler:Languages\BrazilianPortuguese.isl"
Name: "russian"; MessagesFile: "compiler:Languages\Russian.isl"
Name: "turkish"; MessagesFile: "compiler:Languages\Turkish.isl"
Name: "chinesesimplified"; MessagesFile: "languages\ChineseSimplified.isl"
Name: "chinesetraditional"; MessagesFile: "languages\ChineseTraditional.isl"
Name: "korean"; MessagesFile: "languages\Korean.isl"
Name: "croatian"; MessagesFile: "languages\Croatian.isl"
Name: "farsi"; MessagesFile: "languages\Farsi.isl"
Name: "dutch"; MessagesFile: "compiler:Languages\Dutch.isl"
Name: "hebrew"; MessagesFile: "compiler:Languages\Hebrew.isl"
Name: "polish"; MessagesFile: "compiler:Languages\Polish.isl"
Name: "ukrainian"; MessagesFile: "compiler:Languages\Ukrainian.isl"
Name: "arabic"; MessagesFile: "languages\Arabic.isl"
Name: "thai"; MessagesFile: "languages\Thai.isl"
Name: "bengali"; MessagesFile: "languages\Bengali.islu"
Name: "hindi"; MessagesFile: "languages\Hindi.islu"
Name: "indonesian"; MessagesFile: "languages\Indonesian.isl"
Name: "urdu"; MessagesFile: "languages\Urdu.isl"
Name: "vietnamese"; MessagesFile: "languages\Vietnamese.isl"

[Tasks]
Name: "desktopicon"; Description: "{cm:CreateDesktopIcon}"; GroupDescription: "{cm:AdditionalIcons}"; Flags: unchecked

[Files]
Source: "{#Stage}\*"; DestDir: "{app}"; Flags: ignoreversion recursesubdirs createallsubdirs
Source: "..\icons\jdstudio.ico"; DestDir: "{app}"; Flags: ignoreversion

[Icons]
Name: "{autoprograms}\JDStudio"; Filename: "{app}\jdstudio.exe"; IconFilename: "{app}\jdstudio.ico"
Name: "{autodesktop}\JDStudio"; Filename: "{app}\jdstudio.exe"; IconFilename: "{app}\jdstudio.ico"; Tasks: desktopicon

[Run]
Filename: "{app}\jdstudio.exe"; Description: "{cm:LaunchProgram,JDStudio}"; Flags: nowait postinstall skipifsilent
