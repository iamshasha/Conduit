using System.Text.Json.Nodes;
using Microsoft.UI.Windowing;
using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;
using Microsoft.UI.Xaml.Controls.Primitives;
using Microsoft.UI.Xaml.Media;
using Microsoft.UI.Xaml.Media.Animation;
using Microsoft.UI.Xaml.Shapes;

namespace WebShell.Gui;

/// <summary>
/// Dashboard. Settings-tool silhouette: NavigationView on the left, pages of
/// SettingsCards on the right. Built in code so every label comes from
/// Strings.json and can be swapped live when the language changes.
/// </summary>
sealed class MainWindow : Window
{
    static readonly string[] Pages = ["overview", "sites", "activity", "ai", "turbowarp", "settings"];
    static readonly Dictionary<string, string> PageGlyph = new()
    {
        ["ai"] = "",
        ["overview"] = "", ["sites"] = "", ["activity"] = "",
        ["turbowarp"] = "", ["settings"] = "",
    };

    JsonNode _snap;
    string _page = "overview";
    readonly NavigationView _nav = new()
    {
        PaneDisplayMode = NavigationViewPaneDisplayMode.Auto,
        IsBackButtonVisible = NavigationViewBackButtonVisible.Collapsed,
        IsSettingsVisible = false,
        OpenPaneLength = 260,
        CompactModeThresholdWidth = 640,
        ExpandedModeThresholdWidth = 1000,
    };
    readonly ContentControl _host = new()
    {
        HorizontalContentAlignment = HorizontalAlignment.Stretch,
        VerticalContentAlignment = VerticalAlignment.Stretch,
    };
    readonly TextBlock _titleText = new() { VerticalAlignment = VerticalAlignment.Center };
    readonly ToggleButton _detailToggle = new();
    readonly HashSet<string> _selected = new();  // sites picked for bulk revoke
    bool Detailed => _snap["settings"]?["detailed"]?.GetValue<bool>() == true;
    readonly InfoBar _toast = new() { IsClosable = true, VerticalAlignment = VerticalAlignment.Bottom, Margin = new Thickness(24), MaxWidth = 520 };
    readonly DispatcherTimer _statsTimer = new() { Interval = TimeSpan.FromMilliseconds(1500) };
    readonly DispatcherTimer _toastTimer = new() { Interval = TimeSpan.FromSeconds(4) };

    // Overview live widgets (rebuilt with the page; updated by Stats()).
    readonly List<double> _cpu = [];
    Canvas? _graph;
    TextBlock? _cpuText, _memText, _netText, _batText, _upText, _trackTitle, _trackMeta, _volText;
    ProgressBar? _memBar;
    // detailed-mode widgets
    ItemsControl? _cores;
    TextBlock? _gpuText;
    ProgressBar? _gpuBar;
    Slider? _volume;
    ToggleButton? _mute;
    Button? _playPause;
    bool _suppress;
    DateTime _volumeTouched;

    public MainWindow(JsonNode snapshot)
    {
        _snap = snapshot.DeepClone();
        Title = "WebShell";
        SystemBackdrop = new MicaBackdrop();
        ExtendsContentIntoTitleBar = true;
        AppWindow.TitleBar.PreferredHeightOption = TitleBarHeightOption.Tall;
        AppWindow.SetIcon(System.IO.Path.Combine(AppContext.BaseDirectory, "app.ico"));
        Ui.Size(this, 1120, 780, center: true, minWidthDip: 640, minHeightDip: 500);

        // Windows keeps the caption buttons on the right even for RTL content,
        // so the title bar must not mirror or the title slides under them.
        var titleBar = new Grid { Height = 48, Padding = new Thickness(16, 0, 0, 0), FlowDirection = FlowDirection.LeftToRight };
        var titleRow = new StackPanel { Orientation = Orientation.Horizontal, Spacing = 12 };
        titleRow.Children.Add(new Image
        {
            Source = new Microsoft.UI.Xaml.Media.Imaging.BitmapImage(new Uri(System.IO.Path.Combine(AppContext.BaseDirectory, "app.ico"))),
            Width = 16, Height = 16, VerticalAlignment = VerticalAlignment.Center,
        });
        _titleText.Style = Ui.S("CaptionTextBlockStyle");
        titleRow.Children.Add(_titleText);
        titleBar.Children.Add(titleRow);

        // Detailed-mode toggle lives in the title bar (and in Settings). It sits
        // left of the caption buttons; that strip must stay non-draggable.
        _detailToggle.Content = Ui.Icon("", 15);
        _detailToggle.Padding = new Thickness(8, 4, 8, 4);
        _detailToggle.HorizontalAlignment = HorizontalAlignment.Right;
        _detailToggle.VerticalAlignment = VerticalAlignment.Center;
        _detailToggle.Margin = new Thickness(0, 0, 140, 0);
        _detailToggle.Click += (_, _) => SetDetailed(_detailToggle.IsChecked == true);
        Grid.SetColumn(_detailToggle, 0);
        titleBar.Children.Add(_detailToggle);
        SetTitleBar(titleBar);

        _nav.SelectionChanged += (_, e) =>
        {
            if (e.SelectedItem is not NavigationViewItem { Tag: string tag } picked) return;
            // Menu and footer items live in two lists; make sure exactly one
            // item shows as selected, whichever list the previous one was in.
            foreach (var item in _nav.MenuItems.Concat(_nav.FooterMenuItems).OfType<NavigationViewItem>())
                if (!ReferenceEquals(item, picked)) item.IsSelected = false;
            if (tag == _page) return;
            _page = tag;
            Render();
        };
        // Opaque content region: Mica stays behind the nav pane and title bar,
        // but the scrolling cards no longer force a Mica re-blur every frame
        // (the cause of the settings-scroll lag).
        _host.Background = (Brush)Application.Current.Resources["LayerFillColorDefaultBrush"];
        var contentHost = new Grid();
        contentHost.Children.Add(Ui.X<Border>(
            "<Border Background=\"{ThemeResource SolidBackgroundFillColorBaseBrush}\"/>"));
        contentHost.Children.Add(_host);
        _nav.Content = contentHost;

        var root = new Grid();
        root.RowDefinitions.Add(new RowDefinition { Height = GridLength.Auto });
        root.RowDefinitions.Add(new RowDefinition { Height = new GridLength(1, GridUnitType.Star) });
        root.Children.Add(titleBar);
        Grid.SetRow(_nav, 1);
        root.Children.Add(_nav);
        Grid.SetRow(_toast, 1);
        root.Children.Add(_toast);
        Content = root;

        _statsTimer.Tick += (_, _) => Core.Cmd("stats");
        _toastTimer.Tick += (_, _) => { _toast.IsOpen = false; _toastTimer.Stop(); };
        Activated += (_, e) =>
        {
            // Poll only while someone can see the numbers.
            if (e.WindowActivationState == WindowActivationState.Deactivated) return;
            if (_page == "overview" && !_statsTimer.IsEnabled) { _statsTimer.Start(); Core.Cmd("stats"); }
        };
        Closed += (_, _) => { _statsTimer.Stop(); _toastTimer.Stop(); };

        Ui.ApplyTheme(this, App.Settings?["theme"]?.GetValue<string>());
        Relabel();
    }

    string Setting(string key) => _snap["settings"]?[key]?.GetValue<string>() ?? "";

    // ------------------------------------------------------------ plumbing

    /// New data from the core. Rebuild the page only if something it shows
    /// actually changed — the core echoes a snapshot after every action, and
    /// our own actions are already reflected in the controls.
    public void Update(JsonNode snapshot)
    {
        var before = Slice(_snap);
        _snap = snapshot.DeepClone();
        if (Slice(_snap) != before) Render(animate: false);
    }

    /// The part of a snapshot the current page displays.
    string Slice(JsonNode s) => _page switch
    {
        "overview" => $"{s["elevated"]}|{s["port"]}",
        "sites" => new JsonArray((s["grants"] as JsonArray ?? []).Select(g => (JsonNode?)new JsonObject
        {
            // last_used ticks on every request; not worth a redraw.
            ["origin"] = g?["origin"]?.DeepClone(), ["perms"] = g?["perms"]?.DeepClone(),
            ["launch_allow"] = g?["launch_allow"]?.DeepClone(), ["used"] = g?["used"]?.DeepClone(),
        }).ToArray()).ToJsonString(),
        "activity" => s["activity"]?.ToJsonString() ?? "",
        "turbowarp" => s["ext_url"]?.ToJsonString() ?? "",
        "settings" => $"{s["autostart"]}|{s["protocol"]}|{s["storage"]?.ToJsonString()}",
        _ => "",
    };

    /// Apply a change to our copy first so the core's echo is a no-op.
    void Patch(Action<JsonNode> change) => change(_snap);

    async Task<bool> Confirm(string title, string body, string primary)
    {
        var dlg = new ContentDialog
        {
            XamlRoot = Content.XamlRoot,
            Title = title,
            Content = body,
            PrimaryButtonText = primary,
            CloseButtonText = Loc.T("cancel"),
            DefaultButton = ContentDialogButton.Close,
            RequestedTheme = ((FrameworkElement)Content).ActualTheme,
            FlowDirection = Loc.Rtl ? FlowDirection.RightToLeft : FlowDirection.LeftToRight,
        };
        return await dlg.ShowAsync() == ContentDialogResult.Primary;
    }

    /// Rebuild navigation labels and the current page (language changed).
    void SetDetailed(bool on)
    {
        Patch(s => { (s["settings"] as JsonObject)!["detailed"] = on; });
        SaveSettings(detailed: on);
        _detailToggle.IsChecked = on;
        ToolTipService.SetToolTip(_detailToggle, Loc.T("detailed_mode"));
        if (_page == "overview" || _page == "settings") Render(animate: false);
    }

    public void Relabel()
    {
        _titleText.Text = Loc.T("app");
        _detailToggle.IsChecked = Detailed;
        ToolTipService.SetToolTip(_detailToggle, Loc.T("detailed_mode"));
        Microsoft.UI.Xaml.Automation.AutomationProperties.SetName(_detailToggle, Loc.T("detailed_mode"));
        _nav.MenuItems.Clear();
        _nav.FooterMenuItems.Clear();
        foreach (var p in Pages)
        {
            var item = new NavigationViewItem
            {
                Content = Loc.T(p == "turbowarp" ? "nav_extension" : "nav_" + p),
                Icon = Ui.Icon(PageGlyph[p]),
                Tag = p,
            };
            (p == "settings" ? _nav.FooterMenuItems : _nav.MenuItems).Add(item);
            if (p == _page) _nav.SelectedItem = item;
        }
        Ui.ApplyTheme(this, App.Settings?["theme"]?.GetValue<string>());
        Render();
    }

    public void Toast(string key)
    {
        var text = Loc.T(key);
        // Our own confirmations are keys; anything else is an error text from the core.
        _toast.Severity = key == "copied" || key.StartsWith("toast_") ? InfoBarSeverity.Success : InfoBarSeverity.Error;
        _toast.Title = text;
        _toast.IsOpen = true;
        _toastTimer.Stop();
        _toastTimer.Start();
    }

    void Render(bool animate = true)
    {
        _graph = null;
        _statsTimer.Stop();
        // Keep the scroll position when redrawing the same page in place.
        var offset = !animate && _host.Content is ScrollViewer old ? old.VerticalOffset : 0;
        FrameworkElement page = _page switch
        {
            "sites" => SitesPage(),
            "activity" => ActivityPage(),
            "ai" => AiPage(),
            "turbowarp" => TurboWarpPage(),
            "settings" => SettingsPage(),
            _ => OverviewPage(),
        };
        // Motion only when the user navigates, never on a data refresh.
        if (animate) page.Transitions = [new EntranceThemeTransition { FromVerticalOffset = 12 }];
        _host.Content = page;
        if (offset > 0 && page is ScrollViewer sv)
            sv.Loaded += (_, _) => sv.ChangeView(null, offset, null, disableAnimation: true);
        if (_page == "overview") { _statsTimer.Start(); Core.Cmd("stats"); }
    }

    /// Scrollable column with the Windows Settings content width.
    static (ScrollViewer Scroll, StackPanel Column) Column(string title, string? subtitle = null)
    {
        // Left-aligned, fixed max width. Without the explicit Left the content
        // recentred (and appeared to shift) whenever the vertical scrollbar
        // toggled or the nav pane collapsed.
        var col = new StackPanel { Spacing = 4, MaxWidth = 1000, Padding = new Thickness(36, 8, 36, 36) };
        var h = Ui.Text(title, "TitleTextBlockStyle");
        h.Margin = new Thickness(0, 0, 0, subtitle is null ? 20 : 4);
        col.Children.Add(h);
        if (subtitle is not null)
        {
            var s = Ui.Secondary(subtitle, "BodyTextBlockStyle");
            s.Margin = new Thickness(0, 0, 0, 20);
            col.Children.Add(s);
        }
        // Reserve the scrollbar gutter so expanding an item never shifts content.
        var sv = new ScrollViewer
        {
            Content = col,
            HorizontalContentAlignment = HorizontalAlignment.Left,
            VerticalScrollBarVisibility = ScrollBarVisibility.Visible,
        };
        return (sv, col);
    }

    static TextBlock GroupHeader(string text)
    {
        var t = Ui.Text(text, "BodyStrongTextBlockStyle");
        t.Margin = new Thickness(1, 24, 0, 6);
        return t;
    }

    /// Windows Settings row: icon, header + description, control on the right.
    /// Below 560 px the control drops under the text so nothing is clipped.
    static Border Card(string glyph, string header, string? description = null, UIElement? content = null)
    {
        var g = new Grid { ColumnSpacing = 16, MinHeight = 36 };
        g.ColumnDefinitions.Add(new ColumnDefinition { Width = GridLength.Auto });
        g.ColumnDefinitions.Add(new ColumnDefinition { Width = new GridLength(1, GridUnitType.Star) });
        g.ColumnDefinitions.Add(new ColumnDefinition { Width = GridLength.Auto });
        g.RowDefinitions.Add(new RowDefinition { Height = GridLength.Auto });
        g.RowDefinitions.Add(new RowDefinition { Height = GridLength.Auto });
        var icon = Ui.Icon(glyph, 20);
        icon.VerticalAlignment = VerticalAlignment.Center;
        g.Children.Add(icon);
        var text = new StackPanel { VerticalAlignment = VerticalAlignment.Center };
        text.Children.Add(new TextBlock { Text = header, TextWrapping = TextWrapping.Wrap });
        if (!string.IsNullOrEmpty(description)) text.Children.Add(Ui.Secondary(description));
        Grid.SetColumn(text, 1);
        g.Children.Add(text);
        if (content is FrameworkElement c)
        {
            // The row header is the control's label for screen readers.
            if (c is Control && string.IsNullOrEmpty(Microsoft.UI.Xaml.Automation.AutomationProperties.GetName(c)))
                Microsoft.UI.Xaml.Automation.AutomationProperties.SetName(c, header);
            c.VerticalAlignment = VerticalAlignment.Center;
            c.HorizontalAlignment = HorizontalAlignment.Right;
            Grid.SetColumn(c, 2);
            g.Children.Add(c);
            // Static layout: no per-card SizeChanged handler (those fired layout
            // passes during scroll and made it lag). The content window never
            // gets narrow enough to need the control to wrap under the text.
        }
        var card = Ui.Card(g);
        card.Padding = new Thickness(16, 12, 16, 12);
        return card;
    }

    // ------------------------------------------------------------ overview

    FrameworkElement OverviewPage()
    {
        var (scroll, col) = Column(Loc.T("nav_overview"));
        var elevated = _snap["elevated"]?.GetValue<bool>() == true;
        var port = _snap["port"]?.ToString() ?? "8765";

        if (elevated)
            col.Children.Add(new InfoBar
            {
                IsOpen = true, IsClosable = false, Severity = InfoBarSeverity.Warning,
                Title = Loc.T("elevated"), Message = Loc.T("admin_note"), Margin = new Thickness(0, 0, 0, 8),
            });

        col.Children.Add(Card("", Loc.T("status_running", ("port", port)),
            elevated ? Loc.T("elevated") : Loc.T("not_elevated"),
            elevated ? null : Ui.IconButton("", Loc.T("request_admin"), (_, _) => Core.Cmd("elevate"))));

        // Task Manager's performance graph is the vernacular for "live CPU".
        _cpuText = Ui.Text("—", "SubtitleTextBlockStyle");
        _cpuText.HorizontalAlignment = HorizontalAlignment.Right;
        var head = new Grid();
        head.Children.Add(Ui.Text(Loc.T("cpu"), "BodyStrongTextBlockStyle"));
        head.Children.Add(_cpuText);
        _graph = new Canvas { Height = 140, Margin = new Thickness(0, 12, 0, 0) };
        _graph.SizeChanged += (_, _) => DrawGraph();
        var graphBox = new StackPanel();
        graphBox.Children.Add(head);
        graphBox.Children.Add(_graph);
        if (Detailed)
        {
            var model = _snap["cpu_model"]?.GetValue<string>();
            if (!string.IsNullOrEmpty(model))
            {
                var m = Ui.Secondary(model);
                m.Margin = new Thickness(0, 8, 0, 0);
                graphBox.Children.Add(m);
            }
            _cores = new ItemsControl();
            _cores.ItemsPanel = Ui.X<ItemsPanelTemplate>(
                "<ItemsPanelTemplate><StackPanel Orientation=\"Horizontal\" Spacing=\"3\"/></ItemsPanelTemplate>");
            _cores.Margin = new Thickness(0, 10, 0, 0);
            graphBox.Children.Add(_cores);
        }
        var graphCard = Ui.Card(graphBox);
        graphCard.Margin = new Thickness(0, 4, 0, 0);
        col.Children.Add(graphCard);

        if (Detailed)
        {
            foreach (var g in _snap["gpus"] as JsonArray ?? [])
            {
                var box = new StackPanel { Spacing = 6 };
                var top = new Grid();
                top.Children.Add(Ui.Text(g!["name"]?.GetValue<string>() ?? Loc.T("gpu"), "BodyStrongTextBlockStyle"));
                _gpuText = new TextBlock { HorizontalAlignment = HorizontalAlignment.Right, Text = "—" };
                top.Children.Add(_gpuText);
                box.Children.Add(top);
                _gpuBar = new ProgressBar { Maximum = 100, Value = 0 };
                box.Children.Add(_gpuBar);
                var vram = g["vram"]?.GetValue<double>() ?? 0;
                if (vram > 0)
                    box.Children.Add(Ui.Secondary($"{Loc.T("vram")}: {Ui.Bytes(vram)}"));
                col.Children.Add(Ui.Card(box));
                break; // primary adapter's live usage; extra adapters listed below
            }
            var others = (_snap["gpus"] as JsonArray ?? []).Skip(1).ToList();
            foreach (var g in others)
                col.Children.Add(Card("", g!["name"]?.GetValue<string>() ?? "GPU",
                    (g["vram"]?.GetValue<double>() ?? 0) > 0 ? $"{Loc.T("vram")}: {Ui.Bytes(g["vram"]!.GetValue<double>())}" : null));
        }

        _memText = new TextBlock { HorizontalAlignment = HorizontalAlignment.Right };
        _memBar = new ProgressBar { Width = 200, Maximum = 100, Margin = new Thickness(0, 6, 0, 0) };
        var mem = new StackPanel();
        mem.Children.Add(_memText);
        mem.Children.Add(_memBar);
        col.Children.Add(Card("", Loc.T("memory"), null, mem));
        _netText = new TextBlock();
        col.Children.Add(Card("", Loc.T("network"), null, _netText));
        _batText = new TextBlock();
        col.Children.Add(Card("", Loc.T("battery"), null, _batText));
        _upText = new TextBlock();
        col.Children.Add(Card("", Loc.T("uptime"), null, _upText));

        // Media & volume
        col.Children.Add(GroupHeader(Loc.T("media_title")));
        _trackTitle = Ui.Text(Loc.T("nothing_playing"), "BodyStrongTextBlockStyle");
        _trackTitle.TextTrimming = TextTrimming.CharacterEllipsis;
        _trackTitle.TextWrapping = TextWrapping.NoWrap;
        _trackMeta = Ui.Secondary("");
        var track = new StackPanel { VerticalAlignment = VerticalAlignment.Center };
        track.Children.Add(_trackTitle);
        track.Children.Add(_trackMeta);

        Button Transport(string glyph, string tip, string action)
        {
            var b = new Button { Content = Ui.Icon(glyph), Width = 40, Height = 36, Padding = new Thickness(0) };
            ToolTipService.SetToolTip(b, tip);
            Microsoft.UI.Xaml.Automation.AutomationProperties.SetName(b, tip);
            b.Click += (_, _) => Core.Cmd("media_control", new JsonObject { ["action"] = action });
            return b;
        }
        _playPause = Transport("", Loc.T("play"), "toggle");
        var transport = new StackPanel { Orientation = Orientation.Horizontal, Spacing = 4 };
        transport.Children.Add(Transport("", Loc.T("prev_track"), "prev"));
        transport.Children.Add(_playPause);
        transport.Children.Add(Transport("", Loc.T("next_track"), "next"));

        var mediaRow = new Grid { ColumnSpacing = 16 };
        mediaRow.ColumnDefinitions.Add(new ColumnDefinition { Width = new GridLength(1, GridUnitType.Star) });
        mediaRow.ColumnDefinitions.Add(new ColumnDefinition { Width = GridLength.Auto });
        mediaRow.Children.Add(track);
        Grid.SetColumn(transport, 1);
        mediaRow.Children.Add(transport);

        _mute = new ToggleButton { Content = Ui.Icon(""), Width = 40, Height = 36, Padding = new Thickness(0) };
        ToolTipService.SetToolTip(_mute, Loc.T("mute"));
        Microsoft.UI.Xaml.Automation.AutomationProperties.SetName(_mute, Loc.T("mute"));
        _mute.Click += (_, _) =>
        {
            _volumeTouched = DateTime.UtcNow;
            Core.Cmd("volume_set", new JsonObject { ["muted"] = _mute.IsChecked == true });
            SetMuteIcon(_mute.IsChecked == true);
        };
        _volume = new Slider { Minimum = 0, Maximum = 100, StepFrequency = 1, VerticalAlignment = VerticalAlignment.Center };
        Microsoft.UI.Xaml.Automation.AutomationProperties.SetName(_volume, Loc.T("volume"));
        _volume.ValueChanged += (_, e) =>
        {
            if (_suppress) return;
            _volumeTouched = DateTime.UtcNow;
            if (_volText is not null) _volText.Text = $"{e.NewValue:0}";
            Core.Cmd("volume_set", new JsonObject { ["level"] = e.NewValue });
        };
        _volText = new TextBlock { Width = 32, TextAlignment = TextAlignment.Right, VerticalAlignment = VerticalAlignment.Center };
        var volRow = new Grid { ColumnSpacing = 12, Margin = new Thickness(0, 16, 0, 0) };
        volRow.ColumnDefinitions.Add(new ColumnDefinition { Width = GridLength.Auto });
        volRow.ColumnDefinitions.Add(new ColumnDefinition { Width = new GridLength(1, GridUnitType.Star) });
        volRow.ColumnDefinitions.Add(new ColumnDefinition { Width = GridLength.Auto });
        volRow.Children.Add(_mute);
        Grid.SetColumn(_volume, 1);
        volRow.Children.Add(_volume);
        Grid.SetColumn(_volText, 2);
        volRow.Children.Add(_volText);

        var media = new StackPanel();
        media.Children.Add(mediaRow);
        media.Children.Add(volRow);
        col.Children.Add(Ui.Card(media));

        return scroll;
    }

    void SetMuteIcon(bool muted)
    {
        if (_mute is null) return;
        _mute.Content = Ui.Icon(muted ? "" : "");
        ToolTipService.SetToolTip(_mute, Loc.T(muted ? "unmute" : "mute"));
    }

    public void Stats(JsonNode s)
    {
        if (_graph is null) return;
        var c = Loc.Culture;
        var cpu = s["cpu"]?.GetValue<double>() ?? 0;
        _cpu.Add(cpu);
        if (_cpu.Count > 60) _cpu.RemoveAt(0);
        _cpuText!.Text = $"{cpu:0}%";
        DrawGraph();

        if (_cores is not null && s["cores"] is JsonArray cores)
        {
            var vals = cores.Select(v => v?.GetValue<double>() ?? 0).ToList();
            if (_cores.Items.Count != vals.Count)
            {
                _cores.Items.Clear();
                foreach (var _ in vals)
                    _cores.Items.Add(new Border
                    {
                        Width = 8, Height = 26, VerticalAlignment = VerticalAlignment.Bottom, CornerRadius = new CornerRadius(2),
                        Background = (Brush)Application.Current.Resources["ControlStrongFillColorDefaultBrush"],
                        Child = Ui.With(new Border { VerticalAlignment = VerticalAlignment.Bottom, CornerRadius = new CornerRadius(2),
                            Background = (Brush)Application.Current.Resources["AccentFillColorDefaultBrush"] }, _ => { }),
                    });
            }
            for (var i = 0; i < vals.Count; i++)
                if (_cores.Items[i] is Border bd && bd.Child is Border fill)
                    fill.Height = Math.Max(1, 26 * Math.Clamp(vals[i], 0, 100) / 100);
        }
        if (_gpuBar is not null)
        {
            var g = s["gpu_usage"];
            if (g is not null && g.GetValueKind() == System.Text.Json.JsonValueKind.Number)
            {
                _gpuBar.Value = g.GetValue<double>();
                _gpuText!.Text = $"{g.GetValue<double>():0}%";
            }
        }

        var used = s["mem_used"]?.GetValue<double>() ?? 0;
        var total = s["mem_total"]?.GetValue<double>() ?? 1;
        _memText!.Text = $"{Ui.Bytes(used)} / {Ui.Bytes(total)}";
        _memBar!.Value = 100 * used / total;
        _netText!.Text = $"↓ {Ui.Bytes(s["net_rx"]?.GetValue<double>() ?? 0)}/s   ↑ {Ui.Bytes(s["net_tx"]?.GetValue<double>() ?? 0)}/s";
        _upText!.Text = Ui.Uptime(s["uptime"]?.GetValue<long>() ?? 0);
        var b = s["battery"];
        _batText!.Text = b?["present"]?.GetValue<bool>() != true
            ? Loc.T("no_battery")
            : $"{b["percent"]?.GetValue<int>()}%, " + Loc.T(b["charging"]?.GetValue<bool>() == true ? "charging"
                : b["on_ac"]?.GetValue<bool>() == true ? "on_ac" : "on_battery");

        var m = s["media"];
        if (m?["present"]?.GetValue<bool>() == true)
        {
            var title = m["title"]?.GetValue<string>();
            _trackTitle!.Text = string.IsNullOrWhiteSpace(title) ? Loc.T("nothing_playing") : title;
            var artist = m["artist"]?.GetValue<string>() ?? "";
            var album = m["album"]?.GetValue<string>() ?? "";
            _trackMeta!.Text = string.IsNullOrEmpty(album) ? artist : $"{artist}\n{album}";
            var playing = m["status"]?.GetValue<string>() == "playing";
            _playPause!.Content = Ui.Icon(playing ? "" : "");
            ToolTipService.SetToolTip(_playPause, Loc.T(playing ? "pause" : "play"));
        }
        else
        {
            _trackTitle!.Text = Loc.T("nothing_playing");
            _trackMeta!.Text = "";
        }

        // Don't fight the user's hand on the slider.
        if (s["volume"] is JsonNode v && DateTime.UtcNow - _volumeTouched > TimeSpan.FromSeconds(2))
        {
            _suppress = true;
            _volume!.Value = v["level"]?.GetValue<double>() ?? 0;
            _volText!.Text = $"{_volume.Value:0}";
            var muted = v["muted"]?.GetValue<bool>() == true;
            _mute!.IsChecked = muted;
            SetMuteIcon(muted);
            _suppress = false;
        }
    }

    void DrawGraph()
    {
        if (_graph is null || _graph.ActualWidth <= 0) return;
        double w = _graph.ActualWidth, h = _graph.ActualHeight;
        _graph.Children.Clear();
        // Grid: 4 bands like Task Manager.
        for (var i = 1; i < 4; i++)
        {
            var line = Ui.X<Line>("<Line Stroke=\"{ThemeResource DividerStrokeColorDefaultBrush}\" StrokeThickness=\"1\"/>");
            line.X1 = 0; line.X2 = w; line.Y1 = line.Y2 = h * i / 4;
            _graph.Children.Add(line);
        }
        var frame = Ui.X<Rectangle>("<Rectangle Stroke=\"{ThemeResource ControlStrokeColorDefaultBrush}\" StrokeThickness=\"1\"/>");
        frame.Width = w; frame.Height = h;
        _graph.Children.Add(frame);
        if (_cpu.Count < 2) return;

        var step = w / 59.0;
        var start = w - step * (_cpu.Count - 1);
        var pts = new PointCollection();
        for (var i = 0; i < _cpu.Count; i++)
            pts.Add(new Windows.Foundation.Point(start + i * step, h - h * Math.Clamp(_cpu[i], 0, 100) / 100));
        var fill = Ui.X<Polygon>("<Polygon Fill=\"{ThemeResource AccentFillColorDefaultBrush}\" Opacity=\"0.18\"/>");
        var area = new PointCollection { new(start, h) };
        foreach (var p in pts) area.Add(p);
        area.Add(new Windows.Foundation.Point(w, h));
        fill.Points = area;
        _graph.Children.Add(fill);
        var stroke = Ui.X<Polyline>("<Polyline Stroke=\"{ThemeResource AccentFillColorDefaultBrush}\" StrokeThickness=\"1.5\"/>");
        stroke.Points = pts;
        _graph.Children.Add(stroke);
    }

    // --------------------------------------------------------------- sites

    FrameworkElement SitesPage()
    {
        var (scroll, col) = Column(Loc.T("nav_sites"), Loc.T("sites_hint"));
        var grants = _snap["grants"] as JsonArray ?? [];
        if (grants.Count == 0)
        {
            var empty = Ui.Secondary(Loc.T("sites_none"), "BodyTextBlockStyle");
            empty.Margin = new Thickness(0, 24, 0, 0);
            col.Children.Add(empty);
            return scroll;
        }
        var all = (_snap["perms"] as JsonArray ?? []).Select(n => n!.GetValue<string>()).ToList();
        _selected.RemoveWhere(o => !grants.Any(g => g!["origin"]!.GetValue<string>() == o));

        // Bulk actions across every site.
        var bulk = new StackPanel { Orientation = Orientation.Horizontal, Spacing = 8, Margin = new Thickness(0, 0, 0, 12) };
        var revokeSel = Ui.IconButton("", Loc.T("revoke_selected", ("n", _selected.Count)), async (_, _) =>
        {
            var picked = _selected.ToList();
            if (picked.Count > 0 && await Confirm(Loc.T("revoke"),
                Loc.T("revoke_all_confirm", ("n", picked.Count)), Loc.T("revoke")))
            {
                Core.Cmd("revoke_many", new JsonObject { ["origins"] = new JsonArray(picked.Select(o => (JsonNode?)o).ToArray()) });
                _selected.Clear();
            }
        });
        revokeSel.IsEnabled = _selected.Count > 0;
        var selLabel = (TextBlock)((StackPanel)revokeSel.Content).Children[1];
        bulk.Children.Add(revokeSel);
        bulk.Children.Add(Ui.IconButton("", Loc.T("revoke_all"), async (_, _) =>
        {
            if (await Confirm(Loc.T("revoke_all"), Loc.T("revoke_all_confirm", ("n", grants.Count)), Loc.T("revoke_all")))
                Core.Cmd("revoke_all");
        }));
        col.Children.Add(bulk);

        foreach (var g in grants)
        {
            var origin = g!["origin"]!.GetValue<string>();
            var granted = (g["perms"] as JsonArray ?? []).Select(n => n!.GetValue<string>()).ToHashSet();
            var last = g["last_used"]?.GetValue<long>() ?? 0;
            var used = g["used"]?.GetValue<double>() ?? 0;
            var files = g["files"]?.GetValue<long>() ?? 0;

            // Collapsed header: select box, site, one-line summary, quick actions.
            var pick = new CheckBox { IsChecked = _selected.Contains(origin), MinWidth = 0, Margin = new Thickness(0, 0, 4, 0) };
            Microsoft.UI.Xaml.Automation.AutomationProperties.SetName(pick, Loc.T("select"));
            pick.Click += (_, _) =>
            {
                if (pick.IsChecked == true) _selected.Add(origin); else _selected.Remove(origin);
                revokeSel.IsEnabled = _selected.Count > 0;
                selLabel.Text = Loc.T("revoke_selected", ("n", _selected.Count));
            };
            var who = new StackPanel { VerticalAlignment = VerticalAlignment.Center };
            var name = new TextBlock
            {
                Text = Ui.Host(origin), FontWeight = Microsoft.UI.Text.FontWeights.SemiBold,
                TextTrimming = TextTrimming.CharacterEllipsis, TextWrapping = TextWrapping.NoWrap,
            };
            ToolTipService.SetToolTip(name, origin);
            who.Children.Add(name);
            var permNames = granted.Count == 0 ? Loc.T("none")
                : string.Join(", ", all.Where(granted.Contains).Select(p => Loc.T("perm_" + p)));
            who.Children.Add(Ui.Secondary($"{permNames}\n{Loc.T("storage_used", ("size", Ui.Bytes(used)), ("files", files))}"));

            var quick = new StackPanel { Orientation = Orientation.Horizontal, Spacing = 8, VerticalAlignment = VerticalAlignment.Center };
            var filesBtn = Ui.IconButton("", Loc.T("files"),
                (_, _) => Core.Cmd("browse", new JsonObject { ["origin"] = origin, ["path"] = "" }));
            filesBtn.IsEnabled = files > 0;
            quick.Children.Add(filesBtn);
            quick.Children.Add(Ui.IconButton("", Loc.T("open_sandbox"),
                (_, _) => Core.Cmd("open_sandbox", new JsonObject { ["origin"] = origin })));
            quick.Children.Add(Ui.IconButton("", Loc.T("revoke"), async (_, _) =>
            {
                if (await Confirm(Loc.T("revoke"), Loc.T("revoke_confirm", ("origin", Ui.Host(origin))), Loc.T("revoke")))
                {
                    Core.Cmd("revoke", new JsonObject { ["origin"] = origin });
                    Toast("toast_revoked");
                }
            }));

            var head = new Grid { ColumnSpacing = 10 };
            head.ColumnDefinitions.Add(new ColumnDefinition { Width = GridLength.Auto });
            head.ColumnDefinitions.Add(new ColumnDefinition { Width = new GridLength(1, GridUnitType.Star) });
            head.ColumnDefinitions.Add(new ColumnDefinition { Width = GridLength.Auto });
            head.Children.Add(pick);
            Grid.SetColumn(who, 1);
            head.Children.Add(who);
            Grid.SetColumn(quick, 2);
            head.Children.Add(quick);

            // Expanded body: the editable permission grid + remembered apps + clear.
            var boxes = new List<CheckBox>();
            var repeater = new ItemsRepeater
            {
                Layout = new UniformGridLayout { MinItemWidth = 210, MinColumnSpacing = 8, MinRowSpacing = 0, ItemsStretch = UniformGridLayoutItemsStretch.Fill },
                Margin = new Thickness(0, 4, 0, 0),
            };
            var items = new List<UIElement>();
            foreach (var p in all)
            {
                var label = new StackPanel { Orientation = Orientation.Horizontal, Spacing = 8 };
                label.Children.Add(Ui.Icon(Ui.PermGlyph.GetValueOrDefault(p, ""), 14));
                label.Children.Add(new TextBlock { Text = Loc.T("perm_" + p), TextTrimming = TextTrimming.CharacterEllipsis });
                var box = new CheckBox { Content = label, IsChecked = granted.Contains(p), Tag = p };
                ToolTipService.SetToolTip(box, Loc.T("perm_" + p + "_d"));
                Microsoft.UI.Xaml.Automation.AutomationProperties.SetName(box, Loc.T("perm_" + p));
                Microsoft.UI.Xaml.Automation.AutomationProperties.SetHelpText(box, Loc.T("perm_" + p + "_d"));
                box.Click += (_, _) =>
                {
                    var picked = boxes.Where(b => b.IsChecked == true).Select(b => (string)b.Tag).ToList();
                    var arr = new JsonArray(picked.Select(p => (JsonNode?)p).ToArray());
                    Patch(s =>
                    {
                        var mine = (s["grants"] as JsonArray)?.FirstOrDefault(x => x?["origin"]?.GetValue<string>() == origin);
                        if (mine is not null) mine["perms"] = arr.DeepClone();
                    });
                    Core.Cmd("set_perms", new JsonObject { ["origin"] = origin, ["perms"] = arr });
                };
                boxes.Add(box);
                items.Add(box);
            }
            repeater.ItemsSource = items;

            var body = new StackPanel { Spacing = 10 };
            body.Children.Add(Ui.Text(Loc.T("permissions"), "BodyStrongTextBlockStyle"));
            body.Children.Add(repeater);

            var apps = g["launch_allow"] as JsonArray ?? [];
            if (apps.Count > 0)
            {
                body.Children.Add(Ui.Secondary(Loc.T("remembered_apps")));
                foreach (var a in apps)
                {
                    var path = a!.GetValue<string>();
                    var row = new Grid { ColumnSpacing = 8 };
                    row.ColumnDefinitions.Add(new ColumnDefinition { Width = new GridLength(1, GridUnitType.Star) });
                    row.ColumnDefinitions.Add(new ColumnDefinition { Width = GridLength.Auto });
                    row.Children.Add(new TextBlock
                    {
                        Text = path, FontFamily = new FontFamily("Cascadia Mono, Consolas"), IsTextSelectionEnabled = true,
                        TextTrimming = TextTrimming.CharacterEllipsis, VerticalAlignment = VerticalAlignment.Center,
                    });
                    var forget = new Button { Content = Loc.T("forget") };
                    forget.Click += (_, _) => Core.Cmd("forget_app", new JsonObject { ["origin"] = origin, ["path"] = path });
                    Grid.SetColumn(forget, 1);
                    row.Children.Add(forget);
                    body.Children.Add(row);
                }
            }
            var clear = new Button { Content = Loc.T("clear_data"), IsEnabled = files > 0 };
            clear.Click += async (_, _) =>
            {
                if (await Confirm(Loc.T("clear_data"), Loc.T("clear_site_confirm", ("origin", Ui.Host(origin))), Loc.T("clear_data")))
                    Core.Cmd("clear_site", new JsonObject { ["origin"] = origin });
            };
            // Per-site storage limit: a number box (MB) with Set / Use-default.
            var quotaBytes = g["quota"]?.GetValue<long>();
            var mb = new NumberBox
            {
                Minimum = 1, Maximum = 1024 * 1024, SpinButtonPlacementMode = NumberBoxSpinButtonPlacementMode.Compact,
                Value = quotaBytes.HasValue ? quotaBytes.Value / (1024.0 * 1024.0) : 256,
                MinWidth = 130, Header = Loc.T("size_mb"),
            };
            var setLimit = new Button { Content = Loc.T("size_set") };
            setLimit.Click += (_, _) =>
            {
                var bytes = (long)Math.Max(1, mb.Value) * 1024 * 1024;
                Core.Cmd("set_quota", new JsonObject { ["origin"] = origin, ["bytes"] = bytes });
                Toast("toast_saved");
            };
            var clearLimit = new Button { Content = Loc.T("size_clear"), IsEnabled = quotaBytes.HasValue };
            clearLimit.Click += (_, _) => Core.Cmd("set_quota", new JsonObject { ["origin"] = origin });
            var limitRow = new StackPanel { Orientation = Orientation.Horizontal, Spacing = 8, VerticalAlignment = VerticalAlignment.Bottom };
            limitRow.Children.Add(mb);
            limitRow.Children.Add(setLimit);
            limitRow.Children.Add(clearLimit);
            body.Children.Add(Ui.Secondary(quotaBytes.HasValue
                ? Loc.T("size_custom")
                : Loc.T("size_default", ("size", Ui.Bytes(_snap["storage"]?["default_quota"]?.GetValue<double>() ?? 0)))));
            body.Children.Add(limitRow);

            body.Children.Add(clear);

            // Collapsed by default; the header carries the summary and quick actions.
            var exp = new Expander
            {
                Header = head,
                Content = body,
                HorizontalAlignment = HorizontalAlignment.Stretch,
                HorizontalContentAlignment = HorizontalAlignment.Stretch,
                Margin = new Thickness(0, 0, 0, 8),
            };
            col.Children.Add(exp);
        }
        return scroll;
    }

    // ------------------------------------------------------------ activity

    FrameworkElement ActivityPage()
    {
        var grid = new Grid { Padding = new Thickness(36, 8, 36, 24), MaxWidth = 1072, RowSpacing = 8, HorizontalAlignment = HorizontalAlignment.Left };
        grid.RowDefinitions.Add(new RowDefinition { Height = GridLength.Auto });
        grid.RowDefinitions.Add(new RowDefinition { Height = GridLength.Auto });
        grid.RowDefinitions.Add(new RowDefinition { Height = new GridLength(1, GridUnitType.Star) });

        var top = new Grid();
        top.Children.Add(Ui.Text(Loc.T("nav_activity"), "TitleTextBlockStyle"));
        var clear = Ui.IconButton("", Loc.T("clear"), (_, _) => Core.Cmd("clear_activity"));
        clear.HorizontalAlignment = HorizontalAlignment.Right;
        top.Children.Add(clear);
        grid.Children.Add(top);

        var rows = _snap["activity"] as JsonArray ?? [];
        if (rows.Count == 0)
        {
            var empty = Ui.Secondary(Loc.T("activity_none"), "BodyTextBlockStyle");
            Grid.SetRow(empty, 1);
            grid.Children.Add(empty);
            return grid;
        }

        Grid Row(params UIElement[] cells)
        {
            var g = new Grid { ColumnSpacing = 12, Padding = new Thickness(12, 8, 12, 8) };
            foreach (var w in new[] { 110.0, 2, 2, 1 })
                g.ColumnDefinitions.Add(new ColumnDefinition { Width = w > 10 ? new GridLength(w) : new GridLength(w, GridUnitType.Star) });
            for (var i = 0; i < cells.Length; i++) { Grid.SetColumn((FrameworkElement)cells[i], i); g.Children.Add(cells[i]); }
            return g;
        }
        var header = Row(Ui.Secondary(Loc.T("col_time")), Ui.Secondary(Loc.T("col_site")),
                         Ui.Secondary(Loc.T("col_method")), Ui.Secondary(Loc.T("col_result")));
        Grid.SetRow(header, 1);
        grid.Children.Add(header);

        var list = new ListView { SelectionMode = ListViewSelectionMode.None };
        foreach (var r in rows)
        {
            var ok = r!["ok"]?.GetValue<bool>() == true;
            var result = new StackPanel { Orientation = Orientation.Horizontal, Spacing = 8 };
            var dot = Ui.X<Ellipse>(ok
                ? "<Ellipse Width=\"8\" Height=\"8\" Fill=\"{ThemeResource SystemFillColorSuccessBrush}\"/>"
                : "<Ellipse Width=\"8\" Height=\"8\" Fill=\"{ThemeResource SystemFillColorCriticalBrush}\"/>");
            dot.VerticalAlignment = VerticalAlignment.Center;
            result.Children.Add(dot);
            result.Children.Add(new TextBlock { Text = ok ? Loc.T("ok") : r["code"]?.GetValue<string>() ?? Loc.T("failed") });
            var ts = DateTimeOffset.FromUnixTimeSeconds(r["ts"]?.GetValue<long>() ?? 0).LocalDateTime;
            var row = Row(
                new TextBlock { Text = ts.ToString("T", Loc.Culture) },
                new TextBlock { Text = Ui.Host(r["origin"]?.GetValue<string>() ?? ""), TextTrimming = TextTrimming.CharacterEllipsis },
                new TextBlock { Text = r["method"]?.GetValue<string>() ?? "", FontFamily = new FontFamily("Cascadia Mono, Consolas") },
                result);
            row.Padding = new Thickness(0, 6, 0, 6);
            list.Items.Add(row);
        }
        var box = Ui.Card(list, 4);
        Grid.SetRow(box, 2);
        grid.Children.Add(box);
        return grid;
    }

    // ----------------------------------------------------------- turbowarp


    // ------------------------------------------------------------------ ai

    ComboBox? _aiModel;
    TextBox? _aiPrompt;
    TextBox? _aiOut;
    Button? _aiRun;
    InfoBar? _aiStatus;

    FrameworkElement AiPage()
    {
        var (scroll, col) = Column(Loc.T("nav_ai"), Loc.T("perm_ai_d"));

        // Server address row.
        var addr = new TextBox
        {
            Text = _snap["settings"]?["ai_endpoint"]?.GetValue<string>() ?? "",
            PlaceholderText = "http://127.0.0.1:11434",
            MinWidth = 260,
        };
        Microsoft.UI.Xaml.Automation.AutomationProperties.SetName(addr, Loc.T("ai_endpoint"));
        var connect = Ui.IconButton("", Loc.T("ai_test"), (_, _) =>
            Core.Cmd("ai_endpoint", new JsonObject { ["url"] = addr.Text }));
        var addrRow = new StackPanel { Orientation = Orientation.Horizontal, Spacing = 8 };
        addrRow.Children.Add(addr);
        addrRow.Children.Add(connect);
        col.Children.Add(Card("", Loc.T("ai_endpoint"), Loc.T("ai_endpoint_hint"), addrRow));

        _aiStatus = new InfoBar { IsClosable = false, IsOpen = true, Severity = InfoBarSeverity.Informational,
            Title = Loc.T("checking"), Margin = new Thickness(0, 12, 0, 12) };
        col.Children.Add(_aiStatus);

        _aiModel = new ComboBox { MinWidth = 260, IsEnabled = false };
        Microsoft.UI.Xaml.Automation.AutomationProperties.SetName(_aiModel, Loc.T("ai_model"));
        col.Children.Add(Card("", Loc.T("ai_model"), null, _aiModel));

        _aiPrompt = new TextBox
        {
            PlaceholderText = Loc.T("ai_prompt"), AcceptsReturn = true, TextWrapping = TextWrapping.Wrap,
            MinHeight = 90, Margin = new Thickness(0, 4, 0, 8),
        };
        Microsoft.UI.Xaml.Automation.AutomationProperties.SetName(_aiPrompt, Loc.T("ai_prompt"));
        col.Children.Add(_aiPrompt);

        _aiRun = new Button { Content = Loc.T("ai_run"), Style = Ui.S("AccentButtonStyle"), IsEnabled = false };
        _aiRun.Click += (_, _) =>
        {
            var model = (_aiModel?.SelectedItem as string) ?? "";
            var prompt = _aiPrompt?.Text ?? "";
            if (model == "" || prompt.Trim() == "") return;
            _aiRun!.IsEnabled = false;
            _aiRun.Content = Loc.T("ai_running");
            if (_aiOut is not null) _aiOut.Text = "";
            Core.Cmd("ai_generate", new JsonObject { ["model"] = model, ["prompt"] = prompt });
        };
        col.Children.Add(_aiRun);

        _aiOut = new TextBox
        {
            IsReadOnly = true, AcceptsReturn = true, TextWrapping = TextWrapping.Wrap,
            MinHeight = 140, Margin = new Thickness(0, 12, 0, 0), IsSpellCheckEnabled = false,
        };
        Microsoft.UI.Xaml.Automation.AutomationProperties.SetName(_aiOut, "output");
        col.Children.Add(_aiOut);

        Core.Cmd("ai_status");
        return scroll;
    }

    public void ShowAiStatus(JsonNode d)
    {
        if (_page != "ai" || _aiStatus is null) return;
        var online = d["online"]?.GetValue<bool>() == true;
        var models = (d["models"] as JsonArray ?? []).Select(m => m!.GetValue<string>()).ToList();
        if (online)
        {
            _aiStatus.Severity = InfoBarSeverity.Success;
            _aiStatus.Title = Loc.T("ai_online", ("n", models.Count));
            _aiStatus.Message = null;
        }
        else
        {
            _aiStatus.Severity = InfoBarSeverity.Warning;
            _aiStatus.Title = Loc.T("ai_offline");
            _aiStatus.Message = Loc.T("ai_none");
        }
        if (_aiModel is not null)
        {
            _aiModel.Items.Clear();
            foreach (var m in models) _aiModel.Items.Add(m);
            if (models.Count > 0) _aiModel.SelectedIndex = 0;
            _aiModel.IsEnabled = models.Count > 0;
        }
        if (_aiRun is not null) _aiRun.IsEnabled = models.Count > 0;
    }

    public void ShowAiResult(JsonNode d)
    {
        if (_page != "ai") return;
        if (_aiRun is not null) { _aiRun.IsEnabled = true; _aiRun.Content = Loc.T("ai_run"); }
        if (_aiOut is null) return;
        _aiOut.Text = d["ok"]?.GetValue<bool>() == true
            ? d["text"]?.GetValue<string>() ?? ""
            : (d["error"]?.GetValue<string>() ?? "error");
    }

    FrameworkElement TurboWarpPage()
    {
        var (scroll, col) = Column(Loc.T("ext_title"), Loc.T("ext_body"));
        var url = _snap["ext_url"]?.GetValue<string>() ?? "";
        var field = new TextBox { Text = url, IsReadOnly = true, FontFamily = new FontFamily("Cascadia Mono, Consolas") };
        Microsoft.UI.Xaml.Automation.AutomationProperties.SetName(field, Loc.T("ext_title"));
        col.Children.Add(field);
        var buttons = new StackPanel { Orientation = Orientation.Horizontal, Spacing = 8, Margin = new Thickness(0, 12, 0, 0) };
        buttons.Children.Add(Ui.IconButton("", Loc.T("ext_open"), (_, _) => Core.Cmd("open_url", new JsonObject
        {
            ["url"] = "https://turbowarp.org/editor?extension=" + Uri.EscapeDataString(url),
        }), accent: true));
        buttons.Children.Add(Ui.IconButton("", Loc.T("ext_copy"), (_, _) =>
        {
            var pkg = new Windows.ApplicationModel.DataTransfer.DataPackage();
            pkg.SetText(url);
            Windows.ApplicationModel.DataTransfer.Clipboard.SetContent(pkg);
            Toast("copied");
        }));
        col.Children.Add(buttons);
        return scroll;
    }

    // ------------------------------------------------------------ settings

    FrameworkElement SettingsPage()
    {
        var (scroll, col) = Column(Loc.T("nav_settings"));

        var lang = new ComboBox { MinWidth = 200 };
        lang.Items.Add(new ComboBoxItem { Content = Loc.T("theme_system"), Tag = "auto" });
        foreach (var (code, name) in Loc.Languages) lang.Items.Add(new ComboBoxItem { Content = name, Tag = code });
        lang.SelectedItem = lang.Items.Cast<ComboBoxItem>().FirstOrDefault(i => (string)i.Tag == (Setting("lang") is "" ? "auto" : Setting("lang")));
        lang.SelectionChanged += (_, _) => { if (lang.SelectedItem is ComboBoxItem { Tag: string v }) SaveSettings(lang: v); };
        col.Children.Add(Card("", Loc.T("settings_language"), null, lang));

        var theme = new ComboBox { MinWidth = 200 };
        foreach (var t in new[] { "system", "light", "dark" }) theme.Items.Add(new ComboBoxItem { Content = Loc.T("theme_" + t), Tag = t });
        // Built-in accent presets, light and dark listed separately.
        foreach (var p in Ui.Presets)
            theme.Items.Add(new ComboBoxItem { Content = $"{p.Name} · {Loc.T("theme_" + p.Base)}", Tag = "preset:" + p.Id });
        // Imported themes, tagged custom:<id>, each carrying its own base + accent.
        foreach (var t in App.ThemeExtras)
        {
            var id = t?["id"]?.GetValue<string>();
            if (id is null) continue;
            var label = t?["name"]?.GetValue<string>() ?? id;
            theme.Items.Add(new ComboBoxItem { Content = $"{label} ({Loc.T("custom")})", Tag = "custom:" + id });
        }
        var curTheme = Setting("theme") is "" ? "system" : Setting("theme");
        theme.SelectedItem = theme.Items.Cast<ComboBoxItem>().FirstOrDefault(i => (string)i.Tag == curTheme);
        theme.SelectionChanged += (_, _) => { if (theme.SelectedItem is ComboBoxItem { Tag: string v }) SaveSettings(theme: v); };
        col.Children.Add(Card("", Loc.T("settings_theme"), null, theme));

        ToggleSwitch Switch(bool on, Action<bool> changed)
        {
            var sw = new ToggleSwitch { IsOn = on, OnContent = Loc.T("on"), OffContent = Loc.T("off") };
            sw.Toggled += (_, _) => changed(sw.IsOn);
            return sw;
        }

        // Custom themes / languages the user imported.
        var import = new StackPanel { Orientation = Orientation.Horizontal, Spacing = 8 };
        import.Children.Add(Ui.IconButton("", Loc.T("import_theme"), async (_, _) => await ImportFile("theme")));
        import.Children.Add(Ui.IconButton("", Loc.T("import_language"), async (_, _) => await ImportFile("lang")));
        col.Children.Add(Card("", Loc.T("custom"), null, import));

        var gap = new Border { Height = 20 };
        col.Children.Add(gap);
        col.Children.Add(Card("", Loc.T("detailed_mode"), null, Switch(Detailed, SetDetailed)));
        col.Children.Add(Card("", Loc.T("settings_tray"), null,
            Switch(_snap["settings"]?["close_to_tray"]?.GetValue<bool>() != false, v => SaveSettings(tray: v))));
        col.Children.Add(Card("", Loc.T("settings_autostart"), null,
            Switch(_snap["autostart"]?.GetValue<bool>() == true, v =>
            {
                // If the registry write fails, the core's snapshot differs and the page redraws with the truth.
                Patch(s => s["autostart"] = v);
                Core.Cmd("autostart", new JsonObject { ["on"] = v });
            })));
        col.Children.Add(Card("", Loc.T("settings_protocol"), null,
            Switch(_snap["protocol"]?.GetValue<bool>() == true, v =>
            {
                Patch(s => s["protocol"] = v);
                Core.Cmd("protocol", new JsonObject { ["on"] = v });
            })));

        // Storage
        var storage = _snap["storage"];
        var dir = storage?["dir"]?.GetValue<string>() ?? "";
        var used = storage?["used"]?.GetValue<double>() ?? 0;
        var files = storage?["files"]?.GetValue<long>() ?? 0;
        var cache = storage?["cache"]?.GetValue<double>() ?? 0;
        col.Children.Add(GroupHeader(Loc.T("storage")));
        col.Children.Add(Card("", Loc.T("storage_location"), dir,
            Ui.IconButton("", Loc.T("change_location"), async (_, _) => await ChangeLocation())));
        var clearAll = Ui.IconButton("", Loc.T("clear_all"), async (_, _) =>
        {
            if (await Confirm(Loc.T("clear_all"), Loc.T("clear_all_confirm"), Loc.T("clear_all")))
                Core.Cmd("clear_all_sites");
        });
        clearAll.IsEnabled = files > 0;
        col.Children.Add(Card("", Loc.T("nav_sites"),
            Loc.T("storage_used", ("size", Ui.Bytes(used)), ("files", files)), clearAll));
        col.Children.Add(Card("", Loc.T("clear_cache"), $"{Loc.T("clear_cache_desc")}\n{Ui.Bytes(cache)}",
            Ui.IconButton("", Loc.T("clear_cache"), (_, _) => Core.Cmd("clear_cache"))));
        col.Children.Add(new Border { Height = 20 });

        // Icon-only action; the row already says what it does.
        var open = new Button { Content = Ui.Icon(""), Width = 40, Height = 32, Padding = new Thickness(0) };
        ToolTipService.SetToolTip(open, Loc.T("open_data"));
        Microsoft.UI.Xaml.Automation.AutomationProperties.SetName(open, Loc.T("open_data"));
        open.Click += (_, _) => Core.Cmd("open_data");
        col.Children.Add(Card("", Loc.T("open_data"), _snap["data_dir"]?.GetValue<string>(), open));

        col.Children.Add(GroupHeader(Loc.T("about")));
        _updateInfo = new InfoBar { IsClosable = false, Margin = new Thickness(0, 0, 0, 8), Visibility = Visibility.Collapsed };
        col.Children.Add(_updateInfo);
        var check = Ui.IconButton("", Loc.T("check_updates"), (_, _) =>
        {
            if (_updateInfo is not null)
            {
                _updateInfo.Visibility = Visibility.Visible;
                _updateInfo.Severity = InfoBarSeverity.Informational;
                _updateInfo.IsOpen = true;
                _updateInfo.Title = Loc.T("checking");
                _updateInfo.Message = null;
                _updateInfo.ActionButton = null;
            }
            Core.Cmd("check_updates");
        });
        col.Children.Add(Card("", "WebShell", _snap["version"]?.GetValue<string>(), check));

        col.Children.Add(Card("", Loc.T("quit"), null,
            Ui.With(new Button { Content = Loc.T("quit") }, b => b.Click += (_, _) => Core.Cmd("quit"))));
        return scroll;
    }

    InfoBar? _updateInfo;
    FilesWindow? _files;

    public void ShowBrowse(JsonNode d)
    {
        var origin = d["origin"]?.GetValue<string>() ?? "";
        if (_files is null || _files.Origin != origin)
        {
            _files?.Close();
            _files = new FilesWindow(origin);
            _files.Closed += (_, _) => _files = null;
            _files.Activate();
            // The window's own constructor already requested the first listing.
            return;
        }
        _files.Fill(d);
        Ui.Bring(_files);
    }

    public void ShowUpdate(JsonNode d)
    {
        if (_page != "settings" || _updateInfo is null) return;
        _updateInfo.Visibility = Visibility.Visible;
        _updateInfo.IsOpen = true;
        _updateInfo.ActionButton = null;
        if (d["ok"]?.GetValue<bool>() != true)
        {
            _updateInfo.Severity = InfoBarSeverity.Warning;
            _updateInfo.Title = Loc.T("update_failed", ("reason", d["reason"]?.GetValue<string>() ?? ""));
        }
        else if (d["update_available"]?.GetValue<bool>() == true)
        {
            _updateInfo.Severity = InfoBarSeverity.Success;
            _updateInfo.Title = Loc.T("update_ready", ("latest", d["latest"]?.GetValue<string>() ?? ""));
            var url = d["url"]?.GetValue<string>() ?? "";
            if (!string.IsNullOrEmpty(url))
            {
                var b = new Button { Content = Loc.T("get_update") };
                b.Click += (_, _) => Core.Cmd("open_url", new JsonObject { ["url"] = url });
                _updateInfo.ActionButton = b;
            }
        }
        else
        {
            _updateInfo.Severity = InfoBarSeverity.Informational;
            _updateInfo.Title = Loc.T("up_to_date", ("version", d["current"]?.GetValue<string>() ?? ""));
        }
    }

    async Task ImportFile(string kind)
    {
        try
        {
            var picker = new Windows.Storage.Pickers.FileOpenPicker();
            picker.FileTypeFilter.Add(".json");
            WinRT.Interop.InitializeWithWindow.Initialize(picker, WinRT.Interop.WindowNative.GetWindowHandle(this));
            var file = await picker.PickSingleFileAsync();
            if (file is null) return;
            var text = await Windows.Storage.FileIO.ReadTextAsync(file);
            var body = JsonNode.Parse(text);
            if (body is null) { Toast("not valid JSON"); return; }
            Core.Cmd("import", new JsonObject { ["kind"] = kind, ["body"] = body });
        }
        catch (Exception e) { Toast(e.Message); }
    }

    async Task ChangeLocation()
    {
        Windows.Storage.StorageFolder? folder;
        try
        {
            var picker = new Windows.Storage.Pickers.FolderPicker();
            picker.FileTypeFilter.Add("*");
            WinRT.Interop.InitializeWithWindow.Initialize(picker, WinRT.Interop.WindowNative.GetWindowHandle(this));
            folder = await picker.PickSingleFolderAsync();
        }
        catch (Exception e)
        {
            // The shell picker refuses to open in an elevated process.
            Toast(e.Message);
            return;
        }
        if (folder is null) return;
        var target = System.IO.Path.Combine(folder.Path, "WebShell sites");
        if (await Confirm(Loc.T("change_location"), Loc.T("move_confirm", ("path", target)), Loc.T("change_location")))
            Core.Cmd("move_storage", new JsonObject { ["path"] = folder.Path });
    }

    void SaveSettings(string? lang = null, string? theme = null, bool? tray = null, bool? detailed = null)
    {
        var s = (_snap["settings"] as JsonObject)?.DeepClone() as JsonObject ?? new JsonObject();
        if (lang is not null) s["lang"] = lang;
        if (theme is not null) s["theme"] = theme;
        if (tray is not null) s["close_to_tray"] = tray;
        if (detailed is not null) s["detailed"] = detailed;
        s["lang"] ??= "auto";
        s["theme"] ??= "system";
        s["close_to_tray"] ??= true;
        s["detailed"] ??= false;
        _snap["settings"] = s.DeepClone();
        Core.Cmd("settings", new JsonObject { ["settings"] = s });
    }
}
