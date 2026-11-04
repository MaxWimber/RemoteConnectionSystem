param([int]$Seconds = 8)
# Topmost, non-activating test pattern: colour bars, a grey ramp and a moving square.
Add-Type -AssemblyName System.Windows.Forms, System.Drawing
Add-Type -ReferencedAssemblies System.Windows.Forms, System.Drawing -TypeDefinition @'
using System;
using System.Drawing;
using System.Windows.Forms;
public class PatternForm : Form {
    int tick;
    public PatternForm() {
        FormBorderStyle = FormBorderStyle.None;
        StartPosition = FormStartPosition.Manual;
        Bounds = Screen.PrimaryScreen.Bounds;
        TopMost = true;
        ShowInTaskbar = false;
        DoubleBuffered = true;
        var t = new Timer { Interval = 16 };
        t.Tick += (s, e) => { tick++; Invalidate(); };
        t.Start();
    }
    protected override bool ShowWithoutActivation { get { return true; } }
    protected override CreateParams CreateParams {
        get { var p = base.CreateParams; p.ExStyle |= 0x08000000 | 0x00000008; return p; }
    }
    protected override void OnPaint(PaintEventArgs e) {
        var g = e.Graphics;
        int w = ClientSize.Width, h = ClientSize.Height;
        Color[] bars = { Color.FromArgb(191,191,191), Color.FromArgb(191,191,0), Color.FromArgb(0,191,191),
                         Color.FromArgb(0,191,0), Color.FromArgb(191,0,191), Color.FromArgb(191,0,0), Color.FromArgb(0,0,191) };
        int bw = w / bars.Length;
        for (int i = 0; i < bars.Length; i++)
            using (var b = new SolidBrush(bars[i])) g.FillRectangle(b, i * bw, 0, i == bars.Length - 1 ? w - i * bw : bw, h * 2 / 3);
        for (int x = 0; x < w; x++) {
            int v = x * 255 / (w - 1);
            using (var p = new Pen(Color.FromArgb(v, v, v))) g.DrawLine(p, x, h * 2 / 3, x, h * 5 / 6);
        }
        g.FillRectangle(Brushes.Black, 0, h * 5 / 6, w, h - h * 5 / 6);
        int s = 120, x0 = (tick * 12) % (w - s);
        g.FillRectangle(Brushes.White, x0, h * 5 / 6 + 20, s, s);
        using (var f = new Font("Consolas", 48)) g.DrawString("RCS test " + tick, f, Brushes.Black, 40, 40);
    }
}
'@
$form = New-Object PatternForm
$close = New-Object System.Windows.Forms.Timer
$close.Interval = $Seconds * 1000
$close.Add_Tick({ $form.Close() })
$close.Start()
[System.Windows.Forms.Application]::Run($form)
