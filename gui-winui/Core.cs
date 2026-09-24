using System.Globalization;
using System.IO.Pipes;
using System.Runtime.InteropServices;
using System.Text;
using System.Text.Json.Nodes;
using Microsoft.UI;
using Microsoft.UI.Windowing;
using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;
using Microsoft.UI.Xaml.Markup;
using Microsoft.UI.Xaml.Media;
using Windows.Graphics;

namespace WebShell.Gui;

/// <summary>
/// Newline-delimited JSON over the named pipe the core created for us.
/// Core → GUI: {"type": ..., "data": ...}. GUI → core: {"cmd": ..., ...}.
/// </summary>
static class Core
{
    static NamedPipeClientStream? _pipe;
    static StreamWriter? _writer;
    static readonly object Gate = new();

    public static event Action<string, JsonNode?>? Message;
    public static event Action? Closed;

    public static async Task ConnectAsync(string fullName, string key)
    {
        const string prefix = @"\\.\pipe\";
        var name = fullName.StartsWith(prefix, StringComparison.OrdinalIgnoreCase) ? fullName[prefix.Length..] : fullName;
        _pipe = new NamedPipeClientStream(".", name, PipeDirection.InOut, PipeOptions.Asynchronous);
        await _pipe.ConnectAsync(10_000);
        _writer = new StreamWriter(_pipe, new UTF8Encoding(false)) { AutoFlush = true, NewLine = "\n" };
        Send(new JsonObject { ["cmd"] = "hello", ["key"] = key });
        _ = Task.Run(ReadLoopAsync);
    }

    public static void Send(JsonObject msg)
    {
        lock (Gate)
        {
            try { _writer?.WriteLine(msg.ToJsonString()); }
            catch (IOException) { /* core went away; ReadLoop reports it */ }
        }
    }

    public static void Cmd(string cmd, JsonObject? args = null)
    {
        var o = args ?? new JsonObject();
        o["cmd"] = cmd;
        Send(o);
    }

    static async Task ReadLoopAsync()
    {
        using var reader = new StreamReader(_pipe!, new UTF8Encoding(false));
        string? line;
        while ((line = await reader.ReadLineAsync()) is not null)
        {
            JsonNode? node;
            try { node = JsonNode.Parse(line); } catch { continue; }
            var type = node?["type"]?.GetValue<string>() ?? "";
            var data = node?["data"]?.DeepClone();
            App.UI.TryEnqueue(() => Message?.Invoke(type, data));
        }
        App.UI.TryEnqueue(() => Closed?.Invoke());
    }
}

/// <summary>Strings.json lookup with English fallback and RTL flag.</summary>
static class Loc
{
    static readonly JsonObject All = LoadAll();
    static JsonObject _cur = (JsonObject)All["en"]!;
    public static string Lang { get; private set; } = "en";
    public static bool Rtl { get; private set; }

    static JsonObject LoadAll()
    {
        using var s = typeof(Loc).Assembly.GetManifestResourceStream("WebShell.Gui.Strings.json")!;
        return (JsonObject)JsonNode.Parse(s)!;
    }

    /// User-imported language packs: id -> {strings...}. Merged over built-ins.
    static readonly Dictionary<string, JsonObject> Custom = new();

    public static void SetCustomLanguages(JsonArray? langs)
    {
        Custom.Clear();
        foreach (var l in langs ?? new JsonArray())
        {
            var id = l?["id"]?.GetValue<string>();
            if (id is not null && l?["strings"] is JsonObject strings)
                Custom[id] = strings;
        }
    }

    public static IEnumerable<(string Code, string Name)> Languages =>
        All.Select(kv => (kv.Key, kv.Value?["_name"]?.GetValue<string>() ?? kv.Key))
            .Concat(Custom.Select(kv => (kv.Key, kv.Value["_name"]?.GetValue<string>() ?? kv.Key)));

    static JsonObject? _customCur;

    public static void Set(string? pref)
    {
        Lang = Pick(pref);
        _customCur = Custom.TryGetValue(Lang, out var c) ? c : null;
        // A custom pack may name its base language; fall back to English.
        var baseLang = _customCur?["base"]?.GetValue<string>();
        _cur = (JsonObject)All[baseLang is not null && All.ContainsKey(baseLang) ? baseLang : (All.ContainsKey(Lang) ? Lang : "en")]!;
        Rtl = (_customCur?["_rtl"] ?? _cur["_rtl"])?.GetValue<bool>() ?? false;
    }

    static string Pick(string? pref)
    {
        if (!string.IsNullOrEmpty(pref) && pref != "auto" && (All.ContainsKey(pref) || Custom.ContainsKey(pref))) return pref;
        var ui = CultureInfo.CurrentUICulture;
        if (ui.Name.StartsWith("zh", StringComparison.OrdinalIgnoreCase))
            return ui.Name.Contains("TW") || ui.Name.Contains("HK") || ui.Name.Contains("MO") || ui.Name.Contains("Hant")
                ? "zh-Hant" : "zh-Hans";
        var two = ui.TwoLetterISOLanguageName;
        return All.ContainsKey(two) ? two : "en";
    }

    public static string T(string key, params (string Name, object Value)[] vars)
    {
        // Imported pack first, then its base language, then English.
        var s = _customCur?[key]?.GetValue<string>()
            ?? _cur[key]?.GetValue<string>()
            ?? All["en"]![key]?.GetValue<string>()
            ?? key;
        foreach (var (n, v) in vars) s = s.Replace("{" + n + "}", v?.ToString() ?? "");
        return s;
    }

    public static CultureInfo Culture
    {
        get
        {
            try { return CultureInfo.GetCultureInfo(Lang); } catch { return CultureInfo.CurrentCulture; }
        }
    }
}

/// <summary>Small helpers for building Fluent UI from code.</summary>
static class Ui
{
    const string Ns = " xmlns=\"http://schemas.microsoft.com/winfx/2006/xaml/presentation\"";

    /// Parse a XAML snippet so <c>{ThemeResource}</c> brushes follow the
    /// element's theme (code-fetched brushes would not).
    public static T X<T>(string xaml) where T : class
    {
        var i = xaml.IndexOfAny([' ', '/', '>']);
        return (T)XamlReader.Load(xaml.Insert(i, Ns));
    }

    public static Style S(string key) => (Style)Application.Current.Resources[key];

    public static TextBlock Text(string text, string style = "BodyTextBlockStyle") =>
        new() { Text = text, Style = S(style), TextWrapping = TextWrapping.Wrap };

    public static TextBlock Secondary(string text, string style = "CaptionTextBlockStyle")
    {
        var t = X<TextBlock>("<TextBlock Foreground=\"{ThemeResource TextFillColorSecondaryBrush}\" TextWrapping=\"Wrap\"/>");
        t.Text = text;
        t.Style = S(style);
        return t;
    }

    /// Standard Fluent card surface.
    public static Border Card(UIElement child, double padding = 16) =>
        With(X<Border>(
            "<Border Background=\"{ThemeResource CardBackgroundFillColorDefaultBrush}\" " +
            "BorderBrush=\"{ThemeResource CardStrokeColorDefaultBrush}\" BorderThickness=\"1\" CornerRadius=\"8\"/>"),
            b => { b.Padding = new Thickness(padding); b.Child = child; });

    public static FontIcon Icon(string glyph, double size = 16) => new() { Glyph = glyph, FontSize = size };

    public static Button IconButton(string glyph, string label, RoutedEventHandler click, bool accent = false)
    {
        var sp = new StackPanel { Orientation = Orientation.Horizontal, Spacing = 8 };
        sp.Children.Add(Icon(glyph, 14));
        sp.Children.Add(new TextBlock { Text = label });
        var b = new Button { Content = sp };
        if (accent) b.Style = S("AccentButtonStyle");
        // Content is an icon+text panel, so name the button explicitly for
        // screen readers (and UI automation).
        Microsoft.UI.Xaml.Automation.AutomationProperties.SetName(b, label);
        ToolTipService.SetToolTip(b, label);
        b.Click += click;
        return b;
    }

    public static T With<T>(T el, Action<T> init) { init(el); return el; }

    [DllImport("user32.dll")] static extern uint GetDpiForWindow(IntPtr hwnd);
    [DllImport("user32.dll")] static extern bool SetForegroundWindow(IntPtr hwnd);
    [DllImport("user32.dll")] static extern bool FlashWindowEx(ref FLASHWINFO fi);

    [StructLayout(LayoutKind.Sequential)]
    struct FLASHWINFO { public uint cbSize; public IntPtr hwnd; public uint dwFlags; public uint uCount; public uint dwTimeout; }

    /// Resize in DIPs (AppWindow.Resize takes physical pixels).
    public static void Size(Window w, double widthDip, double heightDip, bool center,
        double minWidthDip = 0, double minHeightDip = 0)
    {
        var hwnd = Win32Interop.GetWindowFromWindowId(w.AppWindow.Id);
        var scale = GetDpiForWindow(hwnd) / 96.0;
        var size = new SizeInt32((int)(widthDip * scale), (int)(heightDip * scale));
        w.AppWindow.Resize(size);
        if (minWidthDip > 0 || minHeightDip > 0)
            MinSize(w, minWidthDip > 0 ? minWidthDip : widthDip * 0.5, minHeightDip > 0 ? minHeightDip : heightDip * 0.5);
        if (!center) return;
        var area = DisplayArea.GetFromWindowId(w.AppWindow.Id, DisplayAreaFallback.Primary).WorkArea;
        w.AppWindow.Move(new PointInt32(area.X + (area.Width - size.Width) / 2, area.Y + (area.Height - size.Height) / 2));
    }

    /// Enforce a minimum window size. WinUI has no built-in minimum, so clamp
    /// on resize (guarded against the resize we trigger ourselves).
    public static void MinSize(Window w, double minWidthDip, double minHeightDip)
    {
        var busy = false;
        w.SizeChanged += (_, _) =>
        {
            if (busy) return;
            var hwnd = Win32Interop.GetWindowFromWindowId(w.AppWindow.Id);
            var scale = GetDpiForWindow(hwnd) / 96.0;
            var cur = w.AppWindow.Size;
            int mw = (int)(minWidthDip * scale), mh = (int)(minHeightDip * scale);
            if (cur.Width < mw || cur.Height < mh)
            {
                busy = true;
                w.AppWindow.Resize(new SizeInt32(Math.Max(cur.Width, mw), Math.Max(cur.Height, mh)));
                busy = false;
            }
        };
    }

    /// Automated UI tests set this so windows never take keyboard focus from
    /// whoever is using the machine.
    static readonly bool NoActivate = Environment.GetEnvironmentVariable("WEBSHELL_GUI_NOACTIVATE") == "1";

    /// Show a window the normal way (the dashboard).
    public static void Show(Window w)
    {
        if (NoActivate) w.AppWindow.Show(false);
        else w.Activate();
    }

    /// Permission prompts only: push to the front and flash the taskbar
    /// button, since Windows may refuse focus to a background-launched process.
    public static void Bring(Window w)
    {
        if (NoActivate) { w.AppWindow.Show(false); return; }
        var hwnd = Win32Interop.GetWindowFromWindowId(w.AppWindow.Id);
        w.Activate();
        SetForegroundWindow(hwnd);
        var fi = new FLASHWINFO { cbSize = (uint)Marshal.SizeOf<FLASHWINFO>(), hwnd = hwnd, dwFlags = 3 | 12, uCount = 3 };
        FlashWindowEx(ref fi);
    }

    public static ElementTheme Theme(string? setting) => setting switch
    {
        "light" => ElementTheme.Light,
        "dark" => ElementTheme.Dark,
        _ => ElementTheme.Default,
    };

    /// Built-in accent presets. Each colour ships a light and a dark variant so
    /// the two are always chosen separately, never auto-derived.
    public record Preset(string Id, string Name, string Base, string Accent);
    public static readonly Preset[] Presets =
    [
        new("emerald-l", "Emerald", "light", "#2f9e6e"), new("emerald-d", "Emerald", "dark", "#4fd39a"),
        new("ocean-l",   "Ocean",   "light", "#0a84ff"), new("ocean-d",   "Ocean",   "dark", "#4aa8ff"),
        new("violet-l",  "Violet",  "light", "#7c5cff"), new("violet-d",  "Violet",  "dark", "#a48bff"),
        new("rose-l",    "Rose",    "light", "#e0457b"), new("rose-d",    "Rose",    "dark", "#ff6fa3"),
        new("amber-l",   "Amber",   "light", "#c47a1a"), new("amber-d",   "Amber",   "dark", "#f0a94a"),
        new("slate-l",   "Slate",   "light", "#5b6b7a"), new("slate-d",   "Slate",   "dark", "#90a4b8"),
    ];

    /// Resolve a theme setting to its base (light/dark/system) and optional
    /// accent hex — expands built-in "preset:&lt;id&gt;" and imported
    /// "custom:&lt;id&gt;".
    static (string Base, string? Accent) Resolve(string? setting)
    {
        if (setting is not null && setting.StartsWith("preset:"))
        {
            var id = setting["preset:".Length..];
            var p = Array.Find(Presets, x => x.Id == id);
            if (p is not null) return (p.Base, p.Accent);
        }
        if (setting is not null && setting.StartsWith("custom:"))
        {
            var id = setting["custom:".Length..];
            foreach (var t in App.ThemeExtras)
                if (t?["id"]?.GetValue<string>() == id)
                    return (t["base"]?.GetValue<string>() ?? "system", t["accent"]?.GetValue<string>());
        }
        return (setting ?? "system", null);
    }

    public static void ApplyTheme(Window w, string? setting)
    {
        var (baseTheme, accent) = Resolve(setting);
        if (accent is not null) ApplyAccent(accent); else RestoreAccent();
        if (w.Content is FrameworkElement fe)
        {
            fe.RequestedTheme = Theme(baseTheme);
            fe.FlowDirection = Loc.Rtl ? FlowDirection.RightToLeft : FlowDirection.LeftToRight;
        }
        w.AppWindow.TitleBar.PreferredTheme = baseTheme switch
        {
            "light" => TitleBarTheme.Light,
            "dark" => TitleBarTheme.Dark,
            _ => TitleBarTheme.UseDefaultAppMode,
        };
    }

    static readonly string[] AccentKeys =
        ["AccentFillColorDefaultBrush", "AccentFillColorSecondaryBrush", "AccentFillColorTertiaryBrush", "SystemColorControlAccentBrush"];
    static Dictionary<string, object?>? _accentDefaults;

    /// Override the accent brushes the UI uses (buttons, graph, bars). Best-effort:
    /// a malformed hex leaves the system accent untouched. The first call snapshots
    /// the OS defaults so switching back to a plain theme can restore them.
    public static void ApplyAccent(string hex)
    {
        if (!TryColor(hex, out var c)) return;
        var res = Application.Current.Resources;
        _accentDefaults ??= AccentKeys.Append("SystemAccentColor").ToDictionary(k => k, k => res.TryGetValue(k, out var v) ? v : null);
        var brush = new SolidColorBrush(c);
        foreach (var key in AccentKeys) res[key] = brush;
        res["SystemAccentColor"] = c;
    }

    /// Put the OS accent back after a preset/custom theme is deselected.
    public static void RestoreAccent()
    {
        if (_accentDefaults is null) return;
        var res = Application.Current.Resources;
        foreach (var (k, v) in _accentDefaults)
            if (v is not null) res[k] = v;
    }

    static bool TryColor(string hex, out Windows.UI.Color color)
    {
        color = default;
        hex = hex.TrimStart('#');
        if (hex.Length != 6 || !uint.TryParse(hex, System.Globalization.NumberStyles.HexNumber, null, out var v)) return false;
        color = Windows.UI.Color.FromArgb(255, (byte)(v >> 16), (byte)(v >> 8), (byte)v);
        return true;
    }

    public static string Bytes(double n)
    {
        string[] u = ["B", "KB", "MB", "GB", "TB"];
        var i = 0;
        while (n >= 1024 && i < u.Length - 1) { n /= 1024; i++; }
        return (i == 0 ? n.ToString("0", Loc.Culture) : n.ToString("0.0", Loc.Culture)) + " " + u[i];
    }

    public static string Uptime(long s)
    {
        long d = s / 86400, h = s % 86400 / 3600, m = s % 3600 / 60;
        return d > 0 ? $"{d} d {h} h" : h > 0 ? $"{h} h {m} min" : $"{m} min";
    }

    /// Split an origin into (host, rest) for display: "turbowarp.org", "https://turbowarp.org".
    public static string Host(string origin) =>
        Uri.TryCreate(origin, UriKind.Absolute, out var u) ? u.Host : origin;

    public static readonly Dictionary<string, string> PermGlyph = new()
    {
        ["fs"] = "\uE8B7", ["hw"] = "\uE950", ["launch"] = "\uE8A7", ["system"] = "\uE770",
        ["process"] = "\uE9F5", ["power"] = "\uE7E8", ["clipboard"] = "\uE77F", ["notify"] = "\uEA8F",
        ["hostfs"] = "\uEC25", ["crypto"] = "\uE72E", ["ai"] = "\uE99A",
    };
}
