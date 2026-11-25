This fix is targeted at the rendering of the Menu GUI assets on Series S mode specifically. It can be a fix for Series X but is untested as of now.

The game renders the actual game and the Menu GUI as two different layers, so when running in Series S mode, the GUI is trying to render at 4k still and menu assets appear as invisible. This fix should force them to appear above the game layer and should give control for scaling them accordingly.
