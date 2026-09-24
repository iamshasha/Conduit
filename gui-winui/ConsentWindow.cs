using System.Text.Json.Nodes;
using Microsoft.UI.Windowing;
using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;
using Microsoft.UI.Xaml.Input;
using Microsoft.UI.Xaml.Media;
using Microsoft.UI.Xaml.Media.Animation;
using Windows.System;

namespace Conduit.Gui;

/// <summary>
/// One small always-on-top window that works through the queue of pending
/// permission requests. Laid out like a ContentDialog: what is asked on top,
/// a grey command band with Deny / Allow at the bottom.
/// </summary>
sealed class ConsentWindow : Window
{
    readonly List<JsonNode> _queue = [];
    readonly HashSet<long> _answered = [];
    JsonNode? _cur;

    readonly Grid _body = new();
    readonly ContentControl _slot = new() { HorizontalContentAlignment = HorizontalAlignment.Stretch, VerticalContentAlignment = VerticalAlignment.Stretch };
    readonly ProgressBar _bar = new() { Minimum = 0, Maximum = 1, Value = 1 };
    readonly TextBlock _countdown;
    readonly TextBlock _more;
    readonly Button _allow = new() { HorizontalAlignment = HorizontalAlignment.Stretch };
    readonly Button _deny = new() { HorizontalAlignment = HorizontalAlignment.Stretch };
    readonly DispatcherTimer _tick = new() { Interval = TimeSpan.FromMilliseconds(200) };
    DateTime _deadline, _armedAt;
    CheckBox? _remember;
    List<CheckBox> _permBoxes = [];

    public ConsentWindow()
    {
        Title = Loc.T("consent_title");
        SystemBackdrop = new MicaBackdrop();
        ExtendsContentIntoTitleBar = true;
        AppWindow.SetIcon(Path.Combine(AppContext.BaseDirectory, "app.ico"));
        if (AppWindow.Presenter is OverlappedPresenter p)
        {
            p.IsAlwaysOnTop = true;
            p.IsResizable = false;
            p.IsMaximizable = false;
            p.IsMinimizable = false;
        }
        Ui.Size(this, 460, 600, center: true);

        _countdown = Ui.Secondary("");
        _more = Ui.Secondary("");

        // The single orchestrated motion in the app: each request slides in.
        _slot.ContentTransitions = [new ContentThemeTransition { VerticalOffset = 24 }];

        var content = new Grid { Padding = new Thickness(24, 40, 24, 16) };
        content.Children.Add(_slot);

        _deny.Click += (_, _) => Answer(false);
        _allow.Click += (_, _) => Answer(true);
        var esc = new KeyboardAccelerator { Key = VirtualKey.Escape };
        esc.Invoked += (_, e) => { e.Handled = true; Answer(false); };
        _deny.KeyboardAccelerators.Add(esc);

        var buttons = new Grid { ColumnSpacing = 8 };
        buttons.ColumnDefinitions.Add(new ColumnDefinition());
        buttons.ColumnDefinitions.Add(new ColumnDefinition());
        buttons.Children.Add(_deny);
        Grid.SetColumn(_allow, 1);
        buttons.Children.Add(_allow);

        var status = new Grid();
        status.Children.Add(_countdown);
        _more.HorizontalAlignment = HorizontalAlignment.Right;
        status.Children.Add(_more);

        var bandInner = new StackPanel { Spacing = 12, Padding = new Thickness(24, 16, 24, 24) };
        bandInner.Children.Add(status);
        bandInner.Children.Add(buttons);
        var band = Ui.X<Border>(
            "<Border Background=\"{ThemeResource SolidBackgroundFillColorBaseBrush}\" " +
            "BorderBrush=\"{ThemeResource CardStrokeColorDefaultBrush}\" BorderThickness=\"0,1,0,0\"/>");
        var bandStack = new StackPanel();
        bandStack.Children.Add(_bar);
        bandStack.Children.Add(bandInner);
        band.Child = bandStack;

        _body.RowDefinitions.Add(new RowDefinition { Height = new GridLength(1, GridUnitType.Star) });
        _body.RowDefinitions.Add(new RowDefinition { Height = GridLength.Auto });
        _body.Children.Add(content);
        Grid.SetRow(band, 1);
        _body.Children.Add(band);
        Content = _body;
        Ui.ApplyTheme(this, App.Settings?["theme"]?.GetValue<string>());

        _tick.Tick += (_, _) => Tick();
        Closed += (_, _) =>
        {
            _tick.Stop();
            // Closing the window is a "no" to everything still waiting.
            foreach (var r in _queue) Send(r, false);
        };
    }

    public void Add(JsonNode req)
    {
        var id = req["id"]!.GetValue<long>();
        if (_answered.Contains(id) || _queue.Any(r => r["id"]!.GetValue<long>() == id)) return;
        _queue.Add(req.DeepClone());
        if (_cur is null) Show(_queue[0]);
        else UpdateMore();
        Ui.Bring(this);
    }

    public void Remove(long id)
    {
        var r = _queue.FirstOrDefault(q => q["id"]!.GetValue<long>() == id);
        if (r is null) return;
        _answered.Add(id);
        _queue.Remove(r);
        if (ReferenceEquals(r, _cur)) Next();
        else UpdateMore();
    }

    void Next()
    {
        _cur = null;
        if (_queue.Count == 0) { Close(); return; }
        Show(_queue[0]);
    }

    void UpdateMore() => _more.Text = _queue.Count > 1 ? $"+{_queue.Count - 1}" : "";

    void Send(JsonNode req, bool allow)
    {
        var id = req["id"]!.GetValue<long>();
        if (!_answered.Add(id)) return;
        var perms = new JsonArray();
        if (allow)
        {
            if (req["kind"]?.GetValue<string>() == "pair")
                foreach (var b in _permBoxes.Where(b => b.IsChecked == true)) perms.Add((string)b.Tag);
            else if (req["perms"] is JsonArray asked)
                foreach (var p in asked) perms.Add(p!.GetValue<string>());
        }
        Core.Cmd("consent", new JsonObject
        {
            ["id"] = id,
            ["allow"] = allow,
            ["perms"] = perms,
            ["remember"] = allow && _remember?.IsChecked == true,
        });
    }

    void Answer(bool allow)
    {
        if (_cur is null) return;
        // Anti-clickjacking: Allow is inert until the prompt has been visible a moment.
        if (allow && DateTime.UtcNow < _armedAt) return;
        Send(_cur, allow);
        _queue.Remove(_cur);
        Next();
    }

    void Tick()
    {
        var left = (_deadline - DateTime.UtcNow).TotalSeconds;
        if (left <= 0) { Answer(false); return; }
        _countdown.Text = Loc.T("expires_in", ("n", (int)Math.Ceiling(left)));
        _bar.Value = left / (_cur?["expires_in"]?.GetValue<double>() ?? 60);
        _allow.IsEnabled = DateTime.UtcNow >= _armedAt && (_cur?["kind"]?.GetValue<string>() != "pair" || _permBoxes.Any(b => b.IsChecked == true));
    }

    void Show(JsonNode req)
    {
        _cur = req;
        _remember = null;
        _permBoxes = [];
        var kind = req["kind"]?.GetValue<string>() ?? "";
        var origin = req["origin"]?.GetValue<string>() ?? "";
        var host = Ui.Host(origin);
        var danger = kind is "power" or "kill" or "elevate" or "hostwrite";
        // The core sends "verb\npath[\nto\npath]"; first line is the verb phrase.
        var detailText = req["detail"]?.GetValue<string>() ?? "";
        var hostVerb = detailText.Split('\n').FirstOrDefault() ?? "";

        var head = new StackPanel { Spacing = 4 };
        head.Children.Add(Ui.Secondary(Loc.T("consent_title")));
        var hostText = Ui.Text(host, "TitleTextBlockStyle");
        hostText.TextTrimming = TextTrimming.CharacterEllipsis;
        hostText.TextWrapping = TextWrapping.NoWrap;
        ToolTipService.SetToolTip(hostText, origin);
        head.Children.Add(hostText);
        head.Children.Add(Ui.Secondary(origin));

        var sentence = kind switch
        {
            "pair" => Loc.T("consent_pair", ("origin", host)),
            "launch" => Loc.T("consent_launch", ("origin", host)),
            "kill" => Loc.T("consent_kill", ("origin", host)),
            "clipboard" => Loc.T("consent_clipboard", ("origin", host)),
            "elevate" => Loc.T("consent_elevate", ("origin", host)),
            "power" => Loc.T("consent_power_" + (req["data"]?["action"]?.GetValue<string>() ?? ""), ("origin", host)),
            "hostwrite" => Loc.T("consent_hostwrite", ("origin", host), ("verb", hostVerb)),
            _ => Loc.T("consent_title"),
        };

        var page = new Grid { RowSpacing = 16 };
        page.RowDefinitions.Add(new RowDefinition { Height = GridLength.Auto });
        page.RowDefinitions.Add(new RowDefinition { Height = GridLength.Auto });
        page.RowDefinitions.Add(new RowDefinition { Height = new GridLength(1, GridUnitType.Star) });
        page.Children.Add(head);
        var sentenceText = Ui.Text(sentence, "BodyStrongTextBlockStyle");
        Grid.SetRow(sentenceText, 1);
        page.Children.Add(sentenceText);

        FrameworkElement detail;
        if (kind == "pair")
        {
            var list = new StackPanel { Spacing = 4 };
            list.Children.Add(Ui.Secondary(Loc.T("consent_pair_sub"), "BodyTextBlockStyle"));
            foreach (var p in (req["perms"] as JsonArray ?? []).Select(n => n!.GetValue<string>()))
            {
                var row = new StackPanel { Spacing = 2 };
                var title = new StackPanel { Orientation = Orientation.Horizontal, Spacing = 8 };
                title.Children.Add(Ui.Icon(Ui.PermGlyph.GetValueOrDefault(p, ""), 14));
                title.Children.Add(new TextBlock { Text = Loc.T("perm_" + p) });
                row.Children.Add(title);
                row.Children.Add(Ui.Secondary(Loc.T("perm_" + p + "_d")));
                var box = new CheckBox { Content = row, IsChecked = true, Tag = p, Padding = new Thickness(8, 6, 0, 6) };
                // Content is a panel, so give screen readers the words explicitly.
                Microsoft.UI.Xaml.Automation.AutomationProperties.SetName(box, Loc.T("perm_" + p));
                Microsoft.UI.Xaml.Automation.AutomationProperties.SetHelpText(box, Loc.T("perm_" + p + "_d"));
                _permBoxes.Add(box);
                list.Children.Add(box);
            }
            detail = new ScrollViewer { Content = list };
        }
        else
        {
            var stack = new StackPanel { Spacing = 12 };
            // For hostwrite the verb is already in the title; show only the path(s).
            var body = kind == "hostwrite"
                ? string.Join('\n', detailText.Split('\n').Skip(1))
                : detailText;
            if (!string.IsNullOrEmpty(body))
            {
                var code = new TextBlock
                {
                    Text = body,
                    FontFamily = new FontFamily("Cascadia Mono, Consolas"),
                    TextWrapping = TextWrapping.Wrap,
                    IsTextSelectionEnabled = true,
                };
                stack.Children.Add(Ui.Card(code, 12));
            }
            if (danger)
            {
                stack.Children.Add(new InfoBar
                {
                    IsOpen = true,
                    IsClosable = false,
                    Severity = InfoBarSeverity.Warning,
                    Title = kind switch
                    {
                        "elevate" => Loc.T("admin_note"),
                        "hostwrite" => Loc.T("perm_hostfs"),
                        "kill" => Loc.T("perm_process"),
                        _ => Loc.T("perm_power"),
                    },
                });
            }
            if (req["can_remember"]?.GetValue<bool>() == true)
            {
                _remember = new CheckBox { Content = Loc.T("remember_app") };
                stack.Children.Add(_remember);
            }
            detail = stack;
        }
        Grid.SetRow(detail, 2);
        page.Children.Add(detail);

        _slot.Content = page;
        _deny.Content = Loc.T("deny");
        _allow.Content = req["can_remember"]?.GetValue<bool>() == true ? Loc.T("allow_once") : Loc.T("allow");
        _allow.Style = Ui.S("AccentButtonStyle");
        _allow.IsEnabled = false;

        _deadline = DateTime.UtcNow.AddSeconds(req["expires_in"]?.GetValue<double>() ?? 60);
        _armedAt = DateTime.UtcNow.AddMilliseconds(700);
        UpdateMore();
        Tick();
        _tick.Start();
        // Deny, not Allow, holds focus: a stray Enter/Space never grants anything.
        _deny.Focus(FocusState.Programmatic);
    }
}
