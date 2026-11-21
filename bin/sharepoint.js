#!/usr/bin/env node
'use strict';

require('./launch.js').launch('sharepoint', process.env.SHAREPOINT_PERSONA || 'sharepoint');
