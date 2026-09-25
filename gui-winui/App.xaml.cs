using System.Text.Json.Nodes;
using Microsoft.UI.Dispatching;
using Microsoft.UI.Xaml;

namespace Conduit.Gui;

/// <summary>
/// Started by conduit.exe with <c>--pipe NAME --key KEY [--theme T]</c>.
/// Opens windows only when the core asks, and exits after the last one
/// closes (DispatcherShutdownMode.OnLastWindowClose), so nothing lingers.
/// </summary>
public partial class App : Application
{
    public static DispatcherQueue UI { get; private set; } = null!;
    public static JsonNode? Settings { get; private set; }
    /// User-imported themes from the core, kept for the theme picker.
    public static JsonArray ThemeExtras { get; private set; } = new();

    MainWindow? _main;
    ConsentWindow? _consent;

    public App()
    {
        // The app-wide theme can only be chosen before any window exists; the
        // core passes the saved preference so the first frame is already right.
        var theme = Arg("--theme");
        if (theme == "light") RequestedTheme = ApplicationTheme.Light;
        else if (theme == "dark") RequestedTheme = ApplicationTheme.Dark;
        InitializeComponent();
        // WinUI turns managed exceptions into opaque stowed-exception crashes;
        // keep the real message somewhere a user (or we) can read it.
        UnhandledException += (_, e) => Log($"{e.Message}\n{e.Exception}");
    }

    public static void Log(string text)
    {
        try
        {
            File.AppendAllText(Path.Combine(Path.GetTempPath(), "conduit-gui.log"),
                $"[{DateTime.Now:O}] {text}\n");
        }
        catch { /* logging must never crash the UI */ }
    }

    static string? Arg(string name)
    {
        var a = Environment.GetCommandLineArgs();
        var i = Array.IndexOf(a, name);
        return i >= 0 && i + 1 < a.Length ? a[i + 1] : null;
    }

    protected override async void OnLaunched(LaunchActivatedEventArgs args)
    {
        UI = DispatcherQueue.GetForCurrentThread();
        var pipe = Arg("--pipe");
        var key = Arg("--key");
        if (pipe is null || key is null)
        {
            // Started by hand: there is nothing to talk to.
            Exit();
            return;
        }
        Loc.Set(Arg("--lang"));
        Core.Message += OnMessage;
        Core.Closed += Exit;
        try { await Core.ConnectAsync(pipe, key); }
        catch { Exit(); }
    }

    void OnMessage(string type, JsonNode? data)
    {
        // DispatcherQueue callbacks bypass Application.UnhandledException and
        // would crash the process with an opaque stowed exception.
        try { Route(type, data); }
        catch (Exception e) { Log($"{type}: {e}"); }
    }

    void Route(string type, JsonNode? data)
    {
        switch (type)
        {
            case "settings":
                ApplySettings(data);
                break;
            case "open_main":
                ApplyExtras(data);
                ApplySettings(data?["settings"]);
                if (_main is null)
                {
                    _main = new MainWindow(data!);
                    _main.Closed += (_, _) => { _main = null; Core.Cmd("main_closed"); };
                }
                else
                {
                    _main.Update(data!);
                }
                // Opened because the user clicked the tray/launched the app:
                // an ordinary activation, no focus stealing.
                Ui.Show(_main);
                break;
            case "snapshot":
                ApplyExtras(data);
                ApplySettings(data?["settings"]);
                _main?.Update(data!);
                break;
            case "stats":
                _main?.Stats(data!);
                break;
            case "toast":
                _main?.Toast(data?.GetValue<string>() ?? "");
                break;
            case "update":
                _main?.ShowUpdate(data!);
                break;
            case "update_progress":
                _main?.UpdateProgress((int)(data?.GetValue<double>() ?? 0));
                break;
            case "update_error":
                _main?.UpdateError(data?.GetValue<string>() ?? "");
                break;
            case "browse":
                _main?.ShowBrowse(data!);
                break;
            case "ai_status":
                _main?.ShowAiStatus(data!);
                break;
            case "ai_setup":
                _main?.ShowAiSetup(data!);
                break;
            case "ai_result":
                _main?.ShowAiResult(data!);
                break;
            case "consent_add":
                Consent().Add(data!);
                break;
            case "consents":
                if (data is JsonArray list && list.Count > 0)
                    foreach (var r in list) Consent().Add(r!);
                break;
            case "consent_gone":
                _consent?.Remove(data!.GetValue<long>());
                break;
        }
    }

    /// Absorb imported themes/languages that ride along with a snapshot.
    void ApplyExtras(JsonNode? snap)
    {
        var extras = snap?["extras"];
        if (extras is null) return;
        ThemeExtras = (extras["themes"] as JsonArray)?.DeepClone() as JsonArray ?? new();
        Loc.SetCustomLanguages(extras["langs"] as JsonArray);
    }

    void ApplySettings(JsonNode? s)
    {
        if (s is null) return;
        var langChanged = Settings?["lang"]?.GetValue<string>() != s["lang"]?.GetValue<string>();
        Settings = s.DeepClone();
        Loc.Set(s["lang"]?.GetValue<string>());
        var theme = s["theme"]?.GetValue<string>();
        if (_main is not null) Ui.ApplyTheme(_main, theme);
        if (_consent is not null) Ui.ApplyTheme(_consent, theme);
        if (langChanged) _main?.Relabel();
    }

    ConsentWindow Consent()
    {
        if (_consent is null)
        {
            _consent = new ConsentWindow();
            _consent.Closed += (_, _) => _consent = null;
            Ui.Bring(_consent);
        }
        return _consent;
    }
}
