# Adaptive UI Layout Changes

## Overview

The Shellcaster UI has been modified to implement an adaptive panel display system. Instead of completely hiding the details panel when terminal width is insufficient, the system now shows "active panel + one to the right" when there isn't enough space for all 3 panels.

## How the Original 3-Pane UI Worked

The original UI consists of three horizontal panels:

1. **Podcast Menu** (left) - Shows list of podcasts
2. **Episode Menu** (middle) - Shows episodes for selected podcast  
3. **Details Panel** (right) - Shows episode details

### Original Layout Logic

- **Wide terminals** (> 135 columns): Show all 3 panes equally divided
- **Narrow terminals** (≤ 135 columns): Show only 2 panes (podcast + episodes), hide details panel completely

## New Adaptive Layout

### Layout Logic

- **Wide terminals** (> 135 columns): Same as before - all 3 panes
- **Narrow terminals** (≤ 135 columns): Show active panel + one to the right:
  - If **Podcast Menu** active: Show Podcast + Episodes
  - If **Episodes Menu** active: Show Episodes + Details  
  - If **Details Panel** active: Show Episodes + Details

### Key Changes Made

#### 1. Enhanced `ActivePanel` enum
```rust
// Added Clone and PartialEq traits for comparison
#[derive(Debug, Clone, PartialEq)]
enum ActivePanel {
    PodcastMenu,
    EpisodeMenu, 
    DetailsPanel,
}
```

#### 2. New adaptive layout calculation function
```rust
pub fn calculate_adaptive_sizes(n_col: u16, active_panel: &ActivePanel) -> (u16, u16, u16) {
    if n_col > crate::config::DETAILS_PANEL_LENGTH {
        // Full 3-pane layout
        return Self::calculate_sizes(n_col);
    }
    
    // Limited space: show active panel + one to the right
    match active_panel {
        ActivePanel::PodcastMenu => {
            // Show: Podcast + Episodes
            let pod_col = (n_col + 1) / 2;
            let ep_col = n_col + 1 - pod_col;
            (pod_col, ep_col, 0)
        }
        ActivePanel::EpisodeMenu => {
            // Show: Episodes + Details  
            let ep_col = (n_col + 1) / 2;
            let det_col = n_col + 1 - ep_col;
            (0, ep_col, det_col)
        }
        ActivePanel::DetailsPanel => {
            // Show: Episodes + Details
            let ep_col = (n_col + 1) / 2;
            let det_col = n_col + 1 - ep_col;
            (0, ep_col, det_col)
        }
    }
}
```

#### 3. Updated resize logic
The `resize()` function now:
- Uses the new `calculate_adaptive_sizes()` function
- Properly handles panel positioning when some panels are hidden (width = 0)
- Correctly calculates start_x positions for visible panels

#### 4. Dynamic panel switching
Navigation (Left/Right arrows) now triggers re-layout in adaptive mode:
- When changing panels in narrow terminals, `resize()` is automatically called
- This ensures the correct pair of panels is always visible

## Files Modified

- `/src/ui/mod.rs` - Main UI logic, layout calculations, panel navigation
- `/src/types.rs` - Fixed lock binding warnings  
- `/src/ui/menu.rs` - Fixed lock binding warnings

## How to Test

1. Run shellcaster in a wide terminal (>135 columns) - should work as before
2. Resize terminal to narrow width (≤135 columns) 
3. Use Left/Right arrow keys to navigate between panels
4. Observe that you always see the active panel plus one to the right

## Benefits

- **Better space utilization**: Always shows 2 relevant panels instead of hiding details completely
- **Maintained functionality**: Users can still access all information, just with an extra navigation step
- **Smooth transitions**: Panels dynamically switch as user navigates
- **Backward compatibility**: Wide terminals work exactly as before

## Technical Details

- The threshold for adaptive mode is controlled by `DETAILS_PANEL_LENGTH` constant (135 columns)
- Panel widths are split evenly when showing 2 panels
- Details panel creation/destruction is handled automatically
- Active panel focus is maintained during transitions