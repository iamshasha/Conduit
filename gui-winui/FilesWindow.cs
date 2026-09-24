using System.Text.Json.Nodes;
using Microsoft.UI.Windowing;
using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;
using Microsoft.UI.Xaml.Media;

namespace WebShell.Gui;

/// <summary>
/// A small file browser over one site's sandbox. It never touches the disk
/// itself — it asks the core (`browse` / `delete_file`) and renders the
/// replies routed in through <see cref="Fill"/>.
/// </summary>
sealed class FilesWindow : Window
{
    public string Origin { get; }
    readonly BreadcrumbBar _crumbs = new();
    readonly ListView _list = new() { SelectionMode = ListViewSelectionMode.None };
    readonly TextBlock _empty;
    string _path = "";

    public FilesWindow(string origin)
    {
        Origin = origin;
        Title = $"{Loc.T("files")} — {Ui.Host(origin)}";
        SystemBackdrop = new MicaBackdrop();
        ExtendsContentIntoTitleBar = true;
        AppWindow.SetIcon(Path.Combine(AppContext.BaseDirectory, "app.ico"));
        if (AppWindow.Presenter is OverlappedPresenter p) p.IsAlwaysOnTop = false;
        Ui.Size(this, 760, 560, center: true, minWidthDip: 460, minHeightDip: 360);

        _empty = Ui.Secondary(Loc.T("folder_empty"), "BodyTextBlockStyle");
        _empty.Visibility = Visibility.Collapsed;

        _crumbs.ItemClicked += (_, e) =>
        {
            // Index 0 is the site root; deeper indices are path segments.
            var parts = _path.Split('/', StringSplitOptions.RemoveEmptyEntries);
            Navigate(string.Join('/', parts.Take(e.Index)));
        };

        var open = new Button { Content = Loc.T("open_sandbox"), Margin = new Thickness(0, 0, 0, 8) };
        open.Click += (_, _) => Core.Cmd("open_sandbox", new JsonObject { ["origin"] = Origin });

        var root = new Grid { Padding = new Thickness(16, 40, 16, 16), RowSpacing = 8 };
        root.RowDefinitions.Add(new RowDefinition { Height = GridLength.Auto });
        root.RowDefinitions.Add(new RowDefinition { Height = GridLength.Auto });
        root.RowDefinitions.Add(new RowDefinition { Height = new GridLength(1, GridUnitType.Star) });
        root.Children.Add(_crumbs);
        Grid.SetRow(open, 1);
        root.Children.Add(open);
        var box = Ui.Card(_list, 4);
        Grid.SetRow(box, 2);
        root.Children.Add(box);
        Grid.SetRow(_empty, 2);
        root.Children.Add(_empty);
        Content = root;
        Ui.ApplyTheme(this, App.Settings?["theme"]?.GetValue<string>());

        Navigate("");
    }

    public void Navigate(string path)
    {
        _path = path;
        Core.Cmd("browse", new JsonObject { ["origin"] = Origin, ["path"] = path });
    }

    /// Render a `browse` reply for this window's origin.
    public void Fill(JsonNode d)
    {
        _path = d["path"]?.GetValue<string>() ?? "";
        var items = new BreadcrumbBarItem[] { new() { Content = Loc.T("root") } }
            .Concat(_path.Split('/', StringSplitOptions.RemoveEmptyEntries).Select(seg => new BreadcrumbBarItem { Content = seg }))
            .ToArray();
        _crumbs.ItemsSource = items;

        _list.Items.Clear();
        var entries = d["entries"] as JsonArray ?? [];
        _empty.Visibility = entries.Count == 0 ? Visibility.Visible : Visibility.Collapsed;
        foreach (var e in entries)
        {
            var name = e!["name"]?.GetValue<string>() ?? "";
            var isDir = e["dir"]?.GetValue<bool>() == true;
            var rel = e["path"]?.GetValue<string>() ?? "";

            var row = new Grid { ColumnSpacing = 10, Padding = new Thickness(8, 6, 8, 6) };
            row.ColumnDefinitions.Add(new ColumnDefinition { Width = GridLength.Auto });
            row.ColumnDefinitions.Add(new ColumnDefinition { Width = new GridLength(1, GridUnitType.Star) });
            row.ColumnDefinitions.Add(new ColumnDefinition { Width = GridLength.Auto });
            row.ColumnDefinitions.Add(new ColumnDefinition { Width = GridLength.Auto });
            row.Children.Add(Ui.Icon(isDir ? "" : "", 16));
            var label = new TextBlock { Text = name, VerticalAlignment = VerticalAlignment.Center, TextTrimming = TextTrimming.CharacterEllipsis };
            Grid.SetColumn(label, 1);
            row.Children.Add(label);
            if (!isDir)
            {
                var size = new TextBlock
                {
                    Text = Ui.Bytes(e["size"]?.GetValue<double>() ?? 0),
                    Foreground = (Brush)Application.Current.Resources["TextFillColorSecondaryBrush"],
                    VerticalAlignment = VerticalAlignment.Center,
                };
                Grid.SetColumn(size, 2);
                row.Children.Add(size);
            }
            var del = new Button { Content = Ui.Icon("", 14), Padding = new Thickness(6), Margin = new Thickness(8, 0, 0, 0) };
            ToolTipService.SetToolTip(del, Loc.T("delete"));
            Microsoft.UI.Xaml.Automation.AutomationProperties.SetName(del, Loc.T("delete"));
            del.Click += (_, _) =>
            {
                Core.Cmd("delete_file", new JsonObject { ["origin"] = Origin, ["path"] = rel });
                Navigate(_path); // pipe is ordered: this lists the folder after the delete
            };
            Grid.SetColumn(del, 3);
            row.Children.Add(del);

            var item = new ListViewItem { Content = row, HorizontalContentAlignment = HorizontalAlignment.Stretch };
            if (isDir) item.DoubleTapped += (_, _) => Navigate(rel);
            _list.Items.Add(item);
        }
    }
}
